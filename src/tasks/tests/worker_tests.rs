#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests for the task workers themselves, rather than the tasks they run.

use crate::db::task_queue::{backdate_task, ensure_task, TASK_FTS_OPTIMIZE};
use crate::scripting::{
    Event, EventPayload, FeedEntry, FetchSchedule, ScanSummary, ScheduleDecision, ScriptRunner,
    ScriptRunnerHandle,
};
use crate::tasks::{spawn_workers, TaskManagerCommand};
use crate::test::TestBuilder;
use anyhow::Result;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// A script runner that panics whenever it is used, standing in for a bug
/// anywhere in a feed refresh.
struct PanickingRunner;

impl ScriptRunner for PanickingRunner {
    fn dispatch_transform_entry(&self, _: FeedEntry) -> Result<Option<FeedEntry>> {
        panic!("test panic in entry.ingest");
    }
    fn dispatch_schedule(&self, _: FetchSchedule) -> Result<Option<ScheduleDecision>> {
        panic!("test panic in fetch.schedule");
    }
    fn dispatch_observe(&self, _: Event, _: EventPayload) {
        panic!("test panic in an observe event");
    }
    fn dispatch_scan(&self, _: u64, _: Vec<FeedEntry>) -> Result<Option<Vec<Option<FeedEntry>>>> {
        panic!("test panic in a scan");
    }
    fn finish_scan(&self, _: u64, _: Option<ScanSummary>) {}
}

/// Poll `cond` until it holds, failing the test after five seconds.
async fn wait_for(what: &str, mut cond: impl FnMut() -> Result<bool>) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cond()? {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

/// A refresh that panics is recorded against its feed, which backs off,
/// and the worker that ran it goes on to run the next command.
#[tokio::test(flavor = "multi_thread")]
async fn a_panicking_refresh_is_recorded_and_the_worker_survives() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    let db = crate::db::Db::open(&tc.database_path(), Default::default())?;
    let conn = tc.database_conn()?;
    conn.execute(
        "INSERT INTO feeds (title, url) VALUES ('panics', ?1)",
        [tc.example_feed_url()],
    )?;
    let feed_id = conn.last_insert_rowid();
    ensure_task(&conn, TASK_FTS_OPTIMIZE)?;
    let baseline = backdate_task(&conn, TASK_FTS_OPTIMIZE, 10)?;

    let runner = ScriptRunnerHandle::empty();
    runner.set(Some(Arc::new(PanickingRunner) as Arc<dyn ScriptRunner>));
    let (tx, rx) = async_channel::bounded(16);
    let tx = crate::tasks::TaskSender::from(tx);
    let token = CancellationToken::new();
    // A single worker, so the second command only runs if it survived.
    let handles = spawn_workers(
        rx,
        tx.clone(),
        db,
        token.clone(),
        1,
        Arc::new(super::test_metrics()),
        tc.config_dir().to_path_buf(),
        Arc::new(crate::config::ConfigStore::open(
            tc.database_path()
                .with_file_name(crate::config::CONFIG_FILE_NAME),
        )?),
        runner,
        crate::fetcher::Fetcher::in_process()?,
    );

    tx.send(TaskManagerCommand::RefreshFeed {
        feed_id,
        manual: false,
    })
    .await?;
    tx.send(TaskManagerCommand::OptimizeFts).await?;

    wait_for("the second command to run", || {
        let ran: i64 = conn.query_row(
            "SELECT last_run_at FROM task_queue WHERE task_type = ?1",
            [TASK_FTS_OPTIMIZE],
            |row| row.get(0),
        )?;
        Ok(ran > baseline)
    })
    .await?;

    let (error, failures, next_fetch_at): (Option<String>, i64, Option<i64>) = conn.query_row(
        "SELECT last_fetch_error, consecutive_failures, next_fetch_at FROM feeds WHERE id = ?1",
        [feed_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let error = error.expect("the panic should be recorded against the feed");
    assert!(error.contains("panicked"), "{error}");
    assert!(error.contains("test panic"), "{error}");
    assert_eq!(failures, 1);
    assert!(
        next_fetch_at.expect("the feed should be rescheduled") > chrono::Utc::now().timestamp(),
        "the feed should back off rather than be due again at once"
    );

    token.cancel();
    drop(tx);
    for h in handles {
        h.await?;
    }
    Ok(())
}

/// Once an entry's assets have been cached, it is no longer pending.
#[tokio::test(flavor = "multi_thread")]
async fn cached_entry_assets_are_no_longer_pending() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    let db = crate::db::Db::open(&tc.database_path(), Default::default())?;
    let conn = tc.database_conn()?;
    conn.execute(
        "INSERT INTO entries (feed_id, syndication_format, guid, published_at, title, url)
         VALUES (NULL, 'rss', 'g', 0, 't', 'https://example.com/')",
        [],
    )?;
    let entry_id = conn.last_insert_rowid();
    crate::db::pending_assets::add(&conn, entry_id)?;

    let (tx, rx) = async_channel::bounded(16);
    let tx = crate::tasks::TaskSender::from(tx);
    let token = CancellationToken::new();
    let handles = spawn_workers(
        rx,
        tx.clone(),
        db,
        token.clone(),
        1,
        Arc::new(super::test_metrics()),
        tc.config_dir().to_path_buf(),
        Arc::new(crate::config::ConfigStore::open(
            tc.database_path()
                .with_file_name(crate::config::CONFIG_FILE_NAME),
        )?),
        ScriptRunnerHandle::empty(),
        crate::fetcher::Fetcher::in_process()?,
    );
    tx.send(TaskManagerCommand::CacheEntryAssets { entry_id })
        .await?;

    wait_for("the entry to stop being pending", || {
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pending_entry_assets WHERE entry_id = ?1",
            [entry_id],
            |row| row.get(0),
        )?;
        Ok(n == 0)
    })
    .await?;

    token.cancel();
    drop(tx);
    for h in handles {
        h.await?;
    }
    Ok(())
}

/// The integrity check runs, and is recorded so that its daily schedule
/// survives restarts.
#[tokio::test(flavor = "multi_thread")]
async fn the_integrity_check_runs_and_is_recorded() -> Result<()> {
    use crate::db::task_queue::TASK_INTEGRITY_CHECK;

    let tc = TestBuilder::default().init_database().build()?;
    let db = crate::db::Db::open(&tc.database_path(), Default::default())?;
    let conn = tc.database_conn()?;
    ensure_task(&conn, TASK_INTEGRITY_CHECK)?;
    let baseline = backdate_task(&conn, TASK_INTEGRITY_CHECK, 10)?;

    let (tx, rx) = async_channel::bounded(16);
    let tx = crate::tasks::TaskSender::from(tx);
    let token = CancellationToken::new();
    let handles = spawn_workers(
        rx,
        tx.clone(),
        db,
        token.clone(),
        1,
        Arc::new(super::test_metrics()),
        tc.config_dir().to_path_buf(),
        Arc::new(crate::config::ConfigStore::open(
            tc.database_path()
                .with_file_name(crate::config::CONFIG_FILE_NAME),
        )?),
        ScriptRunnerHandle::empty(),
        crate::fetcher::Fetcher::in_process()?,
    );
    tx.send(TaskManagerCommand::IntegrityCheck).await?;

    wait_for("the integrity check to be recorded", || {
        let ran: i64 = conn.query_row(
            "SELECT last_run_at FROM task_queue WHERE task_type = ?1",
            [TASK_INTEGRITY_CHECK],
            |row| row.get(0),
        )?;
        Ok(ran > baseline)
    })
    .await?;

    token.cancel();
    drop(tx);
    for h in handles {
        h.await?;
    }
    Ok(())
}
