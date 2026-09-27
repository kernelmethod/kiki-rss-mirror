/// Persistence for long-running recurring tasks so that their schedules
/// survive server restarts.
use anyhow::{Context, Result};
use rusqlite::Connection;

pub const TASK_FTS_OPTIMIZE: &str = "fts_optimize";
pub const TASK_WAL_CHECKPOINT_ANALYZE: &str = "wal_checkpoint_analyze";
pub const TASK_INCREMENTAL_VACUUM: &str = "incremental_vacuum";

/// Ensure a `task_queue` row exists for `task_type` and return its
/// `last_run_at` timestamp.
///
/// When the row is first created, `last_run_at` defaults to the current
/// time. This anchors the schedule so that a brand-new install isn't
/// treated as immediately overdue.
pub fn ensure_task(conn: &Connection, task_type: &str) -> Result<i64> {
    conn.execute(
        "INSERT OR IGNORE INTO task_queue (task_type) VALUES (?1)",
        [task_type],
    )
    .with_context(|| format!("failed to initialize task_queue row for {}", task_type))?;

    conn.query_row(
        "SELECT last_run_at FROM task_queue WHERE task_type = ?1",
        [task_type],
        |row| row.get(0),
    )
    .with_context(|| format!("failed to read last_run_at for {}", task_type))
}

/// Record that `task_type` ran successfully at the current time.
pub fn record_run(conn: &Connection, task_type: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO task_queue (task_type, last_run_at) VALUES (?1, unixepoch())
         ON CONFLICT(task_type) DO UPDATE SET last_run_at = excluded.last_run_at",
        [task_type],
    )
    .with_context(|| format!("failed to record run for {}", task_type))?;
    Ok(())
}

/// Move `task_type`'s `last_run_at` `secs` seconds into the past and return
/// the new value.
///
/// `last_run_at` has one-second resolution, so tests use this to make a
/// later run observable without sleeping for a second.
#[cfg(test)]
pub(crate) fn backdate_task(conn: &Connection, task_type: &str, secs: i64) -> Result<i64> {
    conn.query_row(
        "UPDATE task_queue SET last_run_at = last_run_at - ?2 WHERE task_type = ?1
         RETURNING last_run_at",
        rusqlite::params![task_type, secs],
        |row| row.get(0),
    )
    .with_context(|| format!("failed to backdate {}", task_type))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::db::ConnectionBuilder;

    #[test]
    fn ensure_task_inserts_row_with_current_time() -> Result<()> {
        let conn = ConnectionBuilder::default().in_memory().create().build()?;
        let before = chrono::Utc::now().timestamp();
        let ts = ensure_task(&conn, "some_task")?;
        let after = chrono::Utc::now().timestamp();
        assert!(ts >= before && ts <= after);
        Ok(())
    }

    #[test]
    fn ensure_task_is_idempotent() -> Result<()> {
        let conn = ConnectionBuilder::default().in_memory().create().build()?;
        ensure_task(&conn, "some_task")?;
        // Backdate the row so that a fresh insert on the second call would
        // be distinguishable from the existing row.
        let first = backdate_task(&conn, "some_task", 10)?;
        let second = ensure_task(&conn, "some_task")?;
        assert_eq!(first, second);
        Ok(())
    }

    #[test]
    fn record_run_updates_timestamp() -> Result<()> {
        let conn = ConnectionBuilder::default().in_memory().create().build()?;
        ensure_task(&conn, "some_task")?;
        let initial = backdate_task(&conn, "some_task", 10)?;
        record_run(&conn, "some_task")?;
        let after = ensure_task(&conn, "some_task")?;
        assert!(after > initial);
        Ok(())
    }

    #[test]
    fn record_run_inserts_row_if_absent() -> Result<()> {
        let conn = ConnectionBuilder::default().in_memory().create().build()?;
        let before = chrono::Utc::now().timestamp();
        record_run(&conn, "never_seen")?;
        let after = chrono::Utc::now().timestamp();

        let ts: i64 = conn.query_row(
            "SELECT last_run_at FROM task_queue WHERE task_type = ?1",
            ["never_seen"],
            |row| row.get(0),
        )?;
        assert!(ts >= before && ts <= after);
        Ok(())
    }

    #[test]
    fn record_run_is_monotonic_under_repeated_calls() -> Result<()> {
        let conn = ConnectionBuilder::default().in_memory().create().build()?;
        record_run(&conn, "some_task")?;
        let first: i64 = conn.query_row(
            "SELECT last_run_at FROM task_queue WHERE task_type = ?1",
            ["some_task"],
            |row| row.get(0),
        )?;
        record_run(&conn, "some_task")?;
        let second: i64 = conn.query_row(
            "SELECT last_run_at FROM task_queue WHERE task_type = ?1",
            ["some_task"],
            |row| row.get(0),
        )?;
        // `unixepoch()` is second-resolution, so `>=` rather than `>`.
        assert!(second >= first);
        Ok(())
    }
}
