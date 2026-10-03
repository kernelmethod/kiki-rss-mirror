#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::db::task_queue::{
    backdate_task, ensure_task, TASK_FTS_OPTIMIZE, TASK_INCREMENTAL_VACUUM,
    TASK_WAL_CHECKPOINT_ANALYZE,
};
use crate::tasks::{run_maintenance, spawn_workers, TaskManagerCommand};
use crate::test::TestBuilder;
use anyhow::Result;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Open the database the way the server does, so WAL-related PRAGMAs
/// behave the same way under test.
fn make_pool(path: &Path) -> Result<crate::db::Db> {
    crate::db::Db::open(path, Default::default())
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
        let conn = pool.connect();
        for i in 0..60 {
            insert_entry(&conn, i, 50)?;
        }
    }

    let conn = pool.connect();
    let segments_before: i64 =
        conn.query_row("SELECT count(*) FROM entries_fts_data", [], |r| r.get(0))?;
    assert!(
        segments_before >= 2,
        "expected multiple FTS5 segments to accumulate, got {segments_before}"
    );

    ensure_task(&conn, TASK_FTS_OPTIMIZE)?;
    // unixepoch() is second-resolution; backdate to ensure an observable advance.
    let before = backdate_task(&conn, TASK_FTS_OPTIMIZE, 10)?;

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
        let conn = pool.connect();
        for i in 0..20 {
            insert_entry(&conn, i, 50)?;
        }
    }

    let conn = pool.connect();
    ensure_task(&conn, TASK_WAL_CHECKPOINT_ANALYZE)?;
    let before = backdate_task(&conn, TASK_WAL_CHECKPOINT_ANALYZE, 10)?;

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
        let conn = pool.connect();
        for i in 0..300 {
            insert_entry(&conn, i, 500)?;
        }
        conn.execute("DELETE FROM entries", [])?;
    }

    let conn = pool.connect();
    let free_before: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    assert!(
        free_before > 0,
        "expected free pages after bulk delete, got {free_before}"
    );

    ensure_task(&conn, TASK_INCREMENTAL_VACUUM)?;
    let before = backdate_task(&conn, TASK_INCREMENTAL_VACUUM, 10)?;

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

    let conn = pool.connect();
    ensure_task(&conn, "synthetic_task")?;
    // Backdate so that a (wrongly) recorded run would be observable.
    let before = backdate_task(&conn, "synthetic_task", 10)?;

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
        let conn = pool.connect();
        for i in 0..10 {
            insert_entry(&conn, i, 100)?;
        }
    }

    // Backdate each task so that its next run is observable.
    let baselines: Vec<(&'static str, i64)> = {
        let conn = pool.connect();
        [
            TASK_FTS_OPTIMIZE,
            TASK_WAL_CHECKPOINT_ANALYZE,
            TASK_INCREMENTAL_VACUUM,
        ]
        .into_iter()
        .map(|t| {
            ensure_task(&conn, t)?;
            Ok((t, backdate_task(&conn, t, 10)?))
        })
        .collect::<Result<_>>()?
    };

    let (tx, rx) = async_channel::bounded(16);
    let tx = crate::tasks::TaskSender::from(tx);
    let token = CancellationToken::new();
    let handles = spawn_workers(
        rx,
        tx.clone(),
        pool.clone(),
        token.clone(),
        1,
        std::sync::Arc::new(super::test_metrics()),
        std::path::PathBuf::from("."),
        std::sync::Arc::new(crate::config::ConfigStore::open(
            tc.database_path()
                .with_file_name(crate::config::CONFIG_FILE_NAME),
        )?),
        crate::scripting::ScriptRunnerHandle::empty(),
        crate::fetcher::Fetcher::in_process()?,
    );

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
        let all_advanced = {
            let conn = pool.connect();
            baselines
                .iter()
                .all(|(t, b)| last_run_at(&conn, t).map(|a| a > *b).unwrap_or(false))
        };
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
