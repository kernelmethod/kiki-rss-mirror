#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::db::task_queue::{
    ensure_task, TASK_FTS_OPTIMIZE, TASK_INCREMENTAL_VACUUM, TASK_WAL_CHECKPOINT_ANALYZE,
};
use crate::tasks::{run_maintenance, spawn_workers, TaskManagerCommand};
use crate::test::TestBuilder;
use anyhow::Result;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::OpenFlags;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Build a connection pool using the same PRAGMA settings the server uses
/// in production, so WAL-related PRAGMAs behave the same way under test.
fn make_pool(path: &Path) -> Result<r2d2::Pool<SqliteConnectionManager>> {
    let manager = SqliteConnectionManager::file(path)
        .with_flags(OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_init(|c| c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;"));
    Ok(r2d2::Pool::new(manager)?)
}

fn insert_entry(conn: &rusqlite::Connection, i: i64, body_size: usize) -> Result<()> {
    conn.execute(
        "INSERT INTO entries
            (feed_id, syndication_format, guid, published_at, title, url, content)
         VALUES (NULL, 'rss', ?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            format!("guid-{i}"),
            i,
            format!("title {i}"),
            format!("https://example.com/{i}"),
            "x".repeat(body_size),
        ],
    )?;
    Ok(())
}

fn last_run_at(conn: &rusqlite::Connection, task_type: &str) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT last_run_at FROM task_queue WHERE task_type = ?1",
        [task_type],
        |row| row.get(0),
    )?)
}

#[test]
fn optimize_fts_success_records_run_and_merges_segments() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    let pool = make_pool(&tc.database_path())?;

    // Each individual INSERT commits as its own FTS5 segment.
    {
        let conn = pool.get()?;
        for i in 0..60 {
            insert_entry(&conn, i, 50)?;
        }
    }

    let conn = pool.get()?;
    let segments_before: i64 =
        conn.query_row("SELECT count(*) FROM entries_fts_data", [], |r| r.get(0))?;
    assert!(
        segments_before >= 2,
        "expected multiple FTS5 segments to accumulate, got {segments_before}"
    );

    ensure_task(&conn, TASK_FTS_OPTIMIZE)?;
    let before = last_run_at(&conn, TASK_FTS_OPTIMIZE)?;
    // unixepoch() is second-resolution; sleep to ensure an observable advance.
    std::thread::sleep(Duration::from_millis(1100));

    run_maintenance(&pool, TASK_FTS_OPTIMIZE, "FTS5 optimize", |c| {
        c.execute(
            "INSERT INTO entries_fts(entries_fts) VALUES ('optimize')",
            [],
        )?;
        Ok(())
    });

    let after = last_run_at(&conn, TASK_FTS_OPTIMIZE)?;
    assert!(after > before, "last_run_at did not advance");

    let segments_after: i64 =
        conn.query_row("SELECT count(*) FROM entries_fts_data", [], |r| r.get(0))?;
    assert!(
        segments_after < segments_before,
        "optimize should reduce FTS5 segment count ({segments_before} -> {segments_after})"
    );
    Ok(())
}

#[test]
fn wal_checkpoint_analyze_success_records_run_and_populates_stats() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    let pool = make_pool(&tc.database_path())?;

    {
        let conn = pool.get()?;
        for i in 0..20 {
            insert_entry(&conn, i, 50)?;
        }
    }

    let conn = pool.get()?;
    ensure_task(&conn, TASK_WAL_CHECKPOINT_ANALYZE)?;
    let before = last_run_at(&conn, TASK_WAL_CHECKPOINT_ANALYZE)?;
    std::thread::sleep(Duration::from_millis(1100));

    run_maintenance(
        &pool,
        TASK_WAL_CHECKPOINT_ANALYZE,
        "WAL checkpoint and ANALYZE",
        |c| {
            c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); ANALYZE;")?;
            Ok(())
        },
    );

    let after = last_run_at(&conn, TASK_WAL_CHECKPOINT_ANALYZE)?;
    assert!(after > before, "last_run_at did not advance");

    // ANALYZE materializes statistics into sqlite_stat1.
    let stat_rows: i64 = conn.query_row("SELECT count(*) FROM sqlite_stat1", [], |r| r.get(0))?;
    assert!(
        stat_rows > 0,
        "ANALYZE should populate sqlite_stat1; got {stat_rows} rows"
    );
    Ok(())
}

#[test]
fn incremental_vacuum_success_records_run_and_reclaims_pages() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    let pool = make_pool(&tc.database_path())?;

    // Insert bulky rows then delete them all, producing free pages for
    // incremental_vacuum to reclaim.
    {
        let conn = pool.get()?;
        for i in 0..300 {
            insert_entry(&conn, i, 500)?;
        }
        conn.execute("DELETE FROM entries", [])?;
    }

    let conn = pool.get()?;
    let free_before: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    assert!(
        free_before > 0,
        "expected free pages after bulk delete, got {free_before}"
    );

    ensure_task(&conn, TASK_INCREMENTAL_VACUUM)?;
    let before = last_run_at(&conn, TASK_INCREMENTAL_VACUUM)?;
    std::thread::sleep(Duration::from_millis(1100));

    run_maintenance(&pool, TASK_INCREMENTAL_VACUUM, "incremental vacuum", |c| {
        c.execute_batch("PRAGMA incremental_vacuum;")?;
        Ok(())
    });

    let after = last_run_at(&conn, TASK_INCREMENTAL_VACUUM)?;
    assert!(after > before, "last_run_at did not advance");

    let free_after: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    assert!(
        free_after < free_before,
        "incremental_vacuum should reduce freelist count ({free_before} -> {free_after})"
    );
    Ok(())
}

#[test]
fn run_maintenance_failure_does_not_record_run() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    let pool = make_pool(&tc.database_path())?;

    let conn = pool.get()?;
    ensure_task(&conn, "synthetic_task")?;
    let before = last_run_at(&conn, "synthetic_task")?;
    std::thread::sleep(Duration::from_millis(1100));

    run_maintenance(&pool, "synthetic_task", "synthetic", |_c| {
        Err(anyhow::anyhow!("synthetic failure"))
    });

    let after = last_run_at(&conn, "synthetic_task")?;
    assert_eq!(after, before, "failed run must not update last_run_at");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn maintenance_commands_end_to_end_via_worker() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    let pool = make_pool(&tc.database_path())?;

    // Seed a handful of entries so each command has something to work on.
    {
        let conn = pool.get()?;
        for i in 0..10 {
            insert_entry(&conn, i, 100)?;
        }
        ensure_task(&conn, TASK_FTS_OPTIMIZE)?;
        ensure_task(&conn, TASK_WAL_CHECKPOINT_ANALYZE)?;
        ensure_task(&conn, TASK_INCREMENTAL_VACUUM)?;
    }

    let baselines: Vec<(&'static str, i64)> = {
        let conn = pool.get()?;
        vec![
            (TASK_FTS_OPTIMIZE, last_run_at(&conn, TASK_FTS_OPTIMIZE)?),
            (
                TASK_WAL_CHECKPOINT_ANALYZE,
                last_run_at(&conn, TASK_WAL_CHECKPOINT_ANALYZE)?,
            ),
            (
                TASK_INCREMENTAL_VACUUM,
                last_run_at(&conn, TASK_INCREMENTAL_VACUUM)?,
            ),
        ]
    };
    tokio::time::sleep(Duration::from_millis(1100)).await;

    let (tx, rx) = async_channel::bounded(16);
    let token = CancellationToken::new();
    let (reload_tx, _) = tokio::sync::watch::channel(());
    let handles = spawn_workers(rx, tx.clone(), pool.clone(), token.clone(), reload_tx, 1);

    tx.send(TaskManagerCommand::OptimizeFts).await?;
    tx.send(TaskManagerCommand::WalCheckpointAnalyze).await?;
    tx.send(TaskManagerCommand::IncrementalVacuum).await?;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if Instant::now() > deadline {
            token.cancel();
            for h in handles {
                h.await.ok();
            }
            panic!("not all maintenance commands were processed within the timeout");
        }
        let conn = pool.get()?;
        let all_advanced = baselines
            .iter()
            .all(|(t, b)| last_run_at(&conn, t).map(|a| a > *b).unwrap_or(false));
        drop(conn);
        if all_advanced {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    token.cancel();
    drop(tx);
    for h in handles {
        h.await.ok();
    }
    Ok(())
}
