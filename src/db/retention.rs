/// Retention policy helpers for cleaning up old entries.
///
/// Entries are kept for as long as their feed still lists them. A refresh
/// that no longer lists an entry marks it dropped ([`mark_dropped`]), and
/// only entries that have been dropped for longer than the policy's
/// `max_age_days` are deleted ([`cleanup_all`], [`cleanup_feed`]). Entries
/// tagged `system:saved` are never deleted, however long ago they were
/// dropped; once unsaved, they are deleted by the next cleanup if they have
/// been dropped for long enough.
///
/// The policy itself is [`crate::config::RetentionSettings`]; callers pass
/// its `max_age_days` in.
use crate::db::tags::SystemTag;
use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{named_params, Connection};

/// SQL condition, on `entries`, that excludes entries tagged with the system
/// tag named by the `:saved_tag` parameter, i.e. [`SystemTag::Saved`].
const NOT_SAVED: &str = "NOT EXISTS (
    SELECT 1 FROM entry_tags et JOIN tags t ON t.id = et.tag_id
    WHERE et.entry_id = entries.id AND t.kind = 'system' AND t.name = :saved_tag)";

/// Mark the entries of `feed_id` whose guid is not in `seen_guids` as
/// dropped from the feed, as of now.
///
/// `seen_guids` must be every guid stored by a complete, successful refresh
/// of the feed. Entries that are already marked keep their original
/// `dropped_at`, so repeated refreshes don't restart the clock.
///
/// Returns the number of newly marked entries.
///
/// # Errors
///
/// Returns an error if the guids cannot be serialized or the update fails.
pub fn mark_dropped(conn: &Connection, feed_id: i64, seen_guids: &[String]) -> Result<usize> {
    let guids = serde_json::to_string(seen_guids).with_context(|| "failed to serialize guids")?;
    let marked = conn
        .execute(
            "UPDATE entries SET dropped_at = ?1
             WHERE feed_id = ?2
               AND dropped_at IS NULL
               AND guid NOT IN (SELECT value FROM json_each(?3))",
            rusqlite::params![Utc::now().timestamp(), feed_id, guids],
        )
        .with_context(|| format!("failed to mark dropped entries for feed {}", feed_id))?;

    Ok(marked)
}

/// Delete entries that were dropped from their feed more than
/// `max_age_days` ago, across all feeds. Saved entries are kept.
///
/// Returns the number of deleted entries. Returns `Ok(0)` if `max_age_days`
/// is `None`, i.e. no retention policy is configured.
pub fn cleanup_all(conn: &Connection, max_age_days: Option<i64>) -> Result<usize> {
    let Some(max_age_days) = max_age_days else {
        return Ok(0);
    };

    let cutoff = cutoff_timestamp(max_age_days);
    let deleted = conn
        .execute(
            &format!(
                "DELETE FROM entries
                 WHERE dropped_at IS NOT NULL AND dropped_at < :cutoff AND {NOT_SAVED}"
            ),
            named_params! { ":cutoff": cutoff, ":saved_tag": SystemTag::Saved.name() },
        )
        .with_context(|| "failed to delete old entries")?;

    Ok(deleted)
}

/// Delete entries that were dropped from feed `feed_id` more than
/// `max_age_days` ago. Saved entries are kept.
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
            &format!(
                "DELETE FROM entries
                 WHERE dropped_at IS NOT NULL AND dropped_at < :cutoff AND feed_id = :feed_id
                   AND {NOT_SAVED}"
            ),
            named_params! {
                ":cutoff": cutoff,
                ":feed_id": feed_id,
                ":saved_tag": SystemTag::Saved.name(),
            },
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
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::db::ConnectionBuilder;

    const DAY: i64 = 86400;

    fn setup() -> Connection {
        let conn = ConnectionBuilder::default()
            .in_memory()
            .create()
            .build()
            .unwrap();
        conn.execute_batch(
            "INSERT INTO feeds (id, title, url) VALUES
                (1, 'one', 'http://example.com/1'),
                (2, 'two', 'http://example.com/2');",
        )
        .unwrap();
        conn
    }

    fn insert_entry(conn: &Connection, feed_id: i64, guid: &str, dropped_at: Option<i64>) {
        conn.execute(
            "INSERT INTO entries (feed_id, syndication_format, guid, published_at, title, url, dropped_at)
             VALUES (?1, 'rss', ?2, 0, 't', 'u', ?3)",
            rusqlite::params![feed_id, guid, dropped_at],
        )
        .unwrap();
    }

    fn dropped_at(conn: &Connection, feed_id: i64, guid: &str) -> Option<i64> {
        conn.query_row(
            "SELECT dropped_at FROM entries WHERE feed_id = ?1 AND guid = ?2",
            rusqlite::params![feed_id, guid],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn guids(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT guid FROM entries ORDER BY guid")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn cutoff_does_not_overflow() {
        // Far enough in the past to match no entry.
        assert!(cutoff_timestamp(i64::MAX) < -(1 << 62));
        let now = Utc::now().timestamp();
        let c = cutoff_timestamp(1);
        assert!((now - 86400 - 5..=now - 86400 + 5).contains(&c));
    }

    #[test]
    fn mark_dropped_marks_only_unseen_entries_of_the_feed() {
        let conn = setup();
        insert_entry(&conn, 1, "a", None);
        insert_entry(&conn, 1, "b", None);
        insert_entry(&conn, 2, "c", None);

        let marked = mark_dropped(&conn, 1, &["a".to_string()]).unwrap();
        assert_eq!(marked, 1);
        assert_eq!(dropped_at(&conn, 1, "a"), None);
        assert!(dropped_at(&conn, 1, "b").is_some());
        // Other feeds are untouched.
        assert_eq!(dropped_at(&conn, 2, "c"), None);
    }

    #[test]
    fn mark_dropped_keeps_original_timestamp() {
        let conn = setup();
        insert_entry(&conn, 1, "a", Some(123));

        let marked = mark_dropped(&conn, 1, &[]).unwrap();
        assert_eq!(marked, 0);
        assert_eq!(dropped_at(&conn, 1, "a"), Some(123));
    }

    #[test]
    fn cleanup_keeps_entries_still_in_feed_regardless_of_age() {
        let conn = setup();
        // `published_at` is 0, i.e. decades old, but the feed still lists it.
        insert_entry(&conn, 1, "current", None);

        assert_eq!(cleanup_all(&conn, Some(1)).unwrap(), 0);
        assert_eq!(cleanup_feed(&conn, 1, Some(1)).unwrap(), 0);
        assert_eq!(guids(&conn), ["current"]);
    }

    #[test]
    fn cleanup_deletes_only_entries_dropped_before_cutoff() {
        let conn = setup();
        let now = Utc::now().timestamp();
        insert_entry(&conn, 1, "old", Some(now - 10 * DAY));
        insert_entry(&conn, 1, "recent", Some(now - DAY));
        insert_entry(&conn, 2, "other-old", Some(now - 10 * DAY));

        // No policy: nothing is deleted.
        assert_eq!(cleanup_all(&conn, None).unwrap(), 0);
        assert_eq!(cleanup_feed(&conn, 1, None).unwrap(), 0);

        assert_eq!(cleanup_feed(&conn, 1, Some(7)).unwrap(), 1);
        assert_eq!(guids(&conn), ["other-old", "recent"]);

        assert_eq!(cleanup_all(&conn, Some(7)).unwrap(), 1);
        assert_eq!(guids(&conn), ["recent"]);
    }

    fn tag_entry(conn: &Connection, feed_id: i64, guid: &str, tag: &str) {
        conn.execute(
            "INSERT INTO entry_tags (entry_id, tag_id)
             SELECT e.id, t.id FROM entries e, tags t
             WHERE e.feed_id = ?1 AND e.guid = ?2 AND t.name = ?3",
            rusqlite::params![feed_id, guid, tag],
        )
        .unwrap();
    }

    #[test]
    fn cleanup_keeps_saved_entries() {
        let conn = setup();
        let old = Utc::now().timestamp() - 10 * DAY;
        insert_entry(&conn, 1, "saved", Some(old));
        insert_entry(&conn, 1, "read", Some(old));
        insert_entry(&conn, 2, "saved-other", Some(old));
        tag_entry(&conn, 1, "saved", "system:saved");
        tag_entry(&conn, 1, "read", "system:read");
        tag_entry(&conn, 2, "saved-other", "system:saved");

        assert_eq!(cleanup_feed(&conn, 1, Some(7)).unwrap(), 1);
        assert_eq!(guids(&conn), ["saved", "saved-other"]);
        assert_eq!(cleanup_all(&conn, Some(7)).unwrap(), 0);
        assert_eq!(guids(&conn), ["saved", "saved-other"]);

        // Once unsaved, the entry is deleted by the next cleanup.
        conn.execute(
            "DELETE FROM entry_tags WHERE tag_id = (SELECT id FROM tags WHERE name = 'system:saved')
               AND entry_id = (SELECT id FROM entries WHERE guid = 'saved')",
            [],
        )
        .unwrap();
        assert_eq!(cleanup_all(&conn, Some(7)).unwrap(), 1);
        assert_eq!(guids(&conn), ["saved-other"]);
    }
}
