/// Keeping track of which entries their feed still lists.
///
/// Entries are kept for as long as their feed still lists them. A refresh
/// that no longer lists an entry marks it dropped ([`mark_dropped`]), and
/// from then on plugins may delete it, with `delete-entries`
/// (see [`crate::plugins::services`]). The bundled `retention` plugin
/// deletes entries that have been dropped for longer than its
/// `max_age_days`, except those tagged `system:saved`.
use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::Connection;

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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::db::ConnectionBuilder;

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
}
