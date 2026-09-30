//! Durable record of the new entries whose assets have yet to be cached.
//!
//! Caching an entry's assets is a [`TaskManagerCommand::CacheEntryAssets`]
//! on the in-memory task queue, which is lost when the queue is full, when
//! the task fails, or when the server stops. So a new entry also gets a row
//! in `pending_entry_assets`, in the transaction that stores it, and the
//! row is removed once its assets have been cached. The server sweeps up
//! rows that are still there when they fall due ([`take_due`]) and queues
//! them again, up to [`MAX_ATTEMPTS`] times.
//!
//! Only new entries need this: a refresh that downloads the feed again
//! queues asset caching for every entry in it anyway.
//!
//! [`TaskManagerCommand::CacheEntryAssets`]: crate::tasks::TaskManagerCommand::CacheEntryAssets

use rusqlite::{params, Connection};

/// How long after an entry is stored, or its caching queued again, it is
/// taken to have been lost if its row is still there.
pub const RETRY_AFTER_SECS: i64 = 15 * 60;

/// How many times an entry's caching is queued again before Kiki gives up
/// on it.
pub const MAX_ATTEMPTS: i64 = 5;

/// Record that the assets of the entry `entry_id` are to be cached.
///
/// An entry that is already pending keeps its row, and its attempts.
///
/// # Errors
///
/// Returns an error if the row cannot be written.
pub fn add(conn: &Connection, entry_id: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO pending_entry_assets (entry_id, next_attempt_at)
         VALUES (?1, unixepoch() + ?2)
         ON CONFLICT (entry_id) DO NOTHING",
        params![entry_id, RETRY_AFTER_SECS],
    )?;
    Ok(())
}

/// Record that the assets of the entry `entry_id` have been cached, or
/// that there was nothing to cache.
///
/// # Errors
///
/// Returns an error if the row cannot be removed.
pub fn done(conn: &Connection, entry_id: i64) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM pending_entry_assets WHERE entry_id = ?1",
        [entry_id],
    )?;
    Ok(())
}

/// What [`take_due`] found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Due {
    /// Entries to queue caching for again, each now counted as one more
    /// attempt and not due again for [`RETRY_AFTER_SECS`].
    pub retry: Vec<i64>,
    /// Entries given up on after [`MAX_ATTEMPTS`], whose rows are gone.
    pub abandoned: Vec<i64>,
}

/// Take up to `limit` entries whose caching has fallen due, in the order
/// they fell due.
///
/// # Errors
///
/// Returns an error if the rows cannot be read or updated.
pub fn take_due(conn: &mut Connection, limit: usize) -> rusqlite::Result<Due> {
    let tx = conn.transaction()?;
    let rows: Vec<(i64, i64)> = tx
        .prepare(
            "SELECT entry_id, attempts FROM pending_entry_assets
             WHERE next_attempt_at <= unixepoch()
             ORDER BY next_attempt_at
             LIMIT ?1",
        )?
        .query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut due = Due::default();
    for (entry_id, attempts) in rows {
        if attempts >= MAX_ATTEMPTS {
            done(&tx, entry_id)?;
            due.abandoned.push(entry_id);
        } else {
            tx.execute(
                "UPDATE pending_entry_assets
                 SET attempts = attempts + 1, next_attempt_at = unixepoch() + ?2
                 WHERE entry_id = ?1",
                params![entry_id, RETRY_AFTER_SECS],
            )?;
            due.retry.push(entry_id);
        }
    }
    tx.commit()?;
    Ok(due)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::db::ConnectionBuilder;
    use anyhow::Result;

    fn setup() -> Result<(Connection, i64)> {
        let conn = ConnectionBuilder::default().in_memory().create().build()?;
        conn.execute(
            "INSERT INTO entries (feed_id, syndication_format, guid, published_at, title, url)
             VALUES (NULL, 'rss', 'g', 0, 't', 'https://example.com/')",
            [],
        )?;
        let id = conn.last_insert_rowid();
        Ok((conn, id))
    }

    /// Make every pending row due.
    fn make_due(conn: &Connection) -> Result<()> {
        conn.execute(
            "UPDATE pending_entry_assets SET next_attempt_at = unixepoch() - 1",
            [],
        )?;
        Ok(())
    }

    #[test]
    fn a_new_row_is_not_due_until_it_could_have_been_lost() -> Result<()> {
        let (mut conn, id) = setup()?;
        add(&conn, id)?;
        assert_eq!(take_due(&mut conn, 10)?, Due::default());
        make_due(&conn)?;
        assert_eq!(take_due(&mut conn, 10)?.retry, vec![id]);
        // Taken rows are not due again straight away.
        assert_eq!(take_due(&mut conn, 10)?, Due::default());
        Ok(())
    }

    #[test]
    fn done_removes_the_row() -> Result<()> {
        let (mut conn, id) = setup()?;
        add(&conn, id)?;
        done(&conn, id)?;
        make_due(&conn)?;
        assert_eq!(take_due(&mut conn, 10)?, Due::default());
        Ok(())
    }

    #[test]
    fn adding_again_keeps_the_attempts() -> Result<()> {
        let (mut conn, id) = setup()?;
        add(&conn, id)?;
        make_due(&conn)?;
        take_due(&mut conn, 10)?;
        add(&conn, id)?;
        let attempts: i64 =
            conn.query_row("SELECT attempts FROM pending_entry_assets", [], |r| {
                r.get(0)
            })?;
        assert_eq!(attempts, 1);
        Ok(())
    }

    #[test]
    fn entries_are_given_up_on_after_the_last_attempt() -> Result<()> {
        let (mut conn, id) = setup()?;
        add(&conn, id)?;
        for _ in 0..MAX_ATTEMPTS {
            make_due(&conn)?;
            assert_eq!(take_due(&mut conn, 10)?.retry, vec![id]);
        }
        make_due(&conn)?;
        let due = take_due(&mut conn, 10)?;
        assert_eq!(due.abandoned, vec![id]);
        assert!(due.retry.is_empty());
        make_due(&conn)?;
        assert_eq!(take_due(&mut conn, 10)?, Due::default());
        Ok(())
    }

    #[test]
    fn deleting_the_entry_deletes_the_row() -> Result<()> {
        let (conn, id) = setup()?;
        add(&conn, id)?;
        conn.execute("DELETE FROM entries WHERE id = ?1", [id])?;
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM pending_entry_assets", [], |r| {
            r.get(0)
        })?;
        assert_eq!(n, 0);
        Ok(())
    }

    #[test]
    fn take_due_honours_the_limit() -> Result<()> {
        let (mut conn, id) = setup()?;
        add(&conn, id)?;
        conn.execute(
            "INSERT INTO entries (feed_id, syndication_format, guid, published_at, title, url)
             VALUES (NULL, 'rss', 'g2', 0, 't', 'https://example.com/2')",
            [],
        )?;
        add(&conn, conn.last_insert_rowid())?;
        make_due(&conn)?;
        assert_eq!(take_due(&mut conn, 1)?.retry.len(), 1);
        assert_eq!(take_due(&mut conn, 10)?.retry.len(), 1);
        Ok(())
    }
}
