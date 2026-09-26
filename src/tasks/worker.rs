use crate::fetcher::Fetcher;
use crate::http::USER_AGENT;
use crate::metrics::Metrics;
use crate::scripting::{ScriptRunner, ScriptRunnerHandle};
use crate::tasks::command::TaskManagerCommand;
use crate::tasks::entry_assets::cache_entry_assets;
use crate::tasks::error::FetchError;
use crate::tasks::error_recording::set_feed_error;
use crate::tasks::fetch::refresh_feed;
use crate::tasks::maintenance::run_maintenance;
use anyhow::Result;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// Set of feed IDs currently being processed by workers.
type InProgressSet = Arc<Mutex<HashSet<i64>>>;

/// RAII guard that removes a feed ID from an in-progress set when dropped.
///
/// This ensures cleanup happens even if the worker panics or returns early.
struct InProgressGuard {
    feed_id: i64,
    set: InProgressSet,
}

impl InProgressGuard {
    /// Try to claim a feed ID. Returns `Some(guard)` if the ID was not already
    /// in the set, `None` if another worker is already processing it.
    fn try_claim(set: &InProgressSet, feed_id: i64) -> Option<Self> {
        let mut locked = set.lock().unwrap_or_else(|e| e.into_inner());
        if locked.insert(feed_id) {
            Some(InProgressGuard {
                feed_id,
                set: Arc::clone(set),
            })
        } else {
            None
        }
    }
}

impl Drop for InProgressGuard {
    fn drop(&mut self) {
        let mut locked = self.set.lock().unwrap_or_else(|e| e.into_inner());
        locked.remove(&self.feed_id);
    }
}

/// Snapshot the current size of an `InProgressSet` for the
/// `kiki_feeds_refresh_in_progress` gauge. Takes the lock briefly.
fn in_progress_len(set: &InProgressSet) -> f64 {
    let locked = set.lock().unwrap_or_else(|e| e.into_inner());
    locked.len() as f64
}

/// Shared state for a pool of workers that process [`TaskManagerCommand`]s.
///
/// Workers pull commands from a shared channel and dispatch events through a
/// server-wide [`ScriptRunnerHandle`]. Separate in-progress sets prevent two workers
/// from refreshing (or cleaning up) the same feed simultaneously.
#[derive(Clone)]
struct Worker {
    rx: async_channel::Receiver<TaskManagerCommand>,
    tx: async_channel::Sender<TaskManagerCommand>,
    pool: Pool<SqliteConnectionManager>,
    token: CancellationToken,
    refresh_in_progress: InProgressSet,
    cleanup_in_progress: InProgressSet,
    metrics: Arc<Metrics>,
    data_dir: PathBuf,
    script_runner: ScriptRunnerHandle,
    fetcher: Fetcher,
}

/// Determine the number of worker tasks to spawn.
pub fn worker_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

/// Spawn multiple worker tasks that pull from a shared channel.
///
/// All workers share a single [`ScriptRunnerHandle`]; reloads are handled centrally by
/// a separate task that listens on `reload_rx` and swaps the runner inside the handle.
/// They also share one [`Fetcher`], through which every feed refresh
/// retrieves and parses its feed.
#[allow(clippy::too_many_arguments)]
pub fn spawn_workers(
    rx: async_channel::Receiver<TaskManagerCommand>,
    tx: async_channel::Sender<TaskManagerCommand>,
    pool: Pool<SqliteConnectionManager>,
    token: CancellationToken,
    num_workers: usize,
    metrics: Arc<Metrics>,
    data_dir: PathBuf,
    script_runner: ScriptRunnerHandle,
    fetcher: Fetcher,
) -> Vec<tokio::task::JoinHandle<Result<()>>> {
    let worker = Worker {
        rx,
        tx,
        pool,
        token,
        refresh_in_progress: Arc::new(Mutex::new(HashSet::new())),
        cleanup_in_progress: Arc::new(Mutex::new(HashSet::new())),
        metrics,
        data_dir,
        script_runner,
        fetcher,
    };
    let mut handles = Vec::with_capacity(num_workers);

    for worker_id in 0..num_workers {
        let worker = worker.clone();

        handles.push(tokio::spawn(run_worker(worker_id, worker)));
    }

    handles
}

/// A single worker loop that pulls commands from the shared channel.
async fn run_worker(worker_id: usize, w: Worker) -> Result<()> {
    // Used for asset caching only; feed fetches go through `w.fetcher`.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(USER_AGENT)
        .build()?;

    loop {
        let command = tokio::select! {
            cmd = w.rx.recv() => {
                match cmd {
                    Ok(c) => c,
                    Err(_) => return Ok(()), // channel closed
                }
            }
            _ = w.token.cancelled() => return Ok(()),
        };

        w.metrics.inc_workers_busy();
        let task_start = Instant::now();

        match command {
            TaskManagerCommand::RefreshFeed(feed_id) => {
                let guard = match InProgressGuard::try_claim(&w.refresh_in_progress, feed_id) {
                    Some(g) => {
                        w.metrics
                            .set_feeds_refresh_in_progress(in_progress_len(&w.refresh_in_progress));
                        g
                    }
                    None => {
                        debug!(
                            "Worker {}: feed {} already in progress, skipping refresh",
                            worker_id, feed_id
                        );
                        w.metrics.record_task_processed(
                            "refresh_feed",
                            "skipped_in_progress",
                            task_start.elapsed().as_secs_f64(),
                        );
                        w.metrics.dec_workers_busy();
                        continue;
                    }
                };

                let runner_snapshot = w.script_runner.current();
                let script_runner: Option<&dyn ScriptRunner> = runner_snapshot.as_deref();

                let outcome = match refresh_feed(
                    &w.fetcher,
                    feed_id,
                    w.pool.clone(),
                    script_runner,
                    &w.metrics,
                    &w.tx,
                )
                .await
                {
                    Ok(()) => "ok",
                    Err(e) => {
                        error!(
                            "An error occurred while refreshing feed {}: {:?}",
                            feed_id, e
                        );
                        if let Ok(conn) = w.pool.get() {
                            set_feed_error(
                                &conn,
                                feed_id,
                                &FetchError::Other {
                                    message: format!("{}", e),
                                },
                                &w.metrics,
                            );
                        }
                        "error"
                    }
                };

                drop(guard);
                w.metrics
                    .set_feeds_refresh_in_progress(in_progress_len(&w.refresh_in_progress));

                w.metrics.record_task_processed(
                    "refresh_feed",
                    outcome,
                    task_start.elapsed().as_secs_f64(),
                );

                // Queue retention cleanup for this feed.
                if let Err(e) = w.tx.try_send(TaskManagerCommand::CleanupFeed(feed_id)) {
                    warn!("Failed to queue cleanup for feed {}: {:?}", feed_id, e);
                } else {
                    w.metrics.record_task_enqueued("cleanup_feed");
                }
            }

            TaskManagerCommand::CleanupFeed(feed_id) => {
                let _guard = match InProgressGuard::try_claim(&w.cleanup_in_progress, feed_id) {
                    Some(g) => g,
                    None => {
                        debug!(
                            "Worker {}: feed {} already in progress, skipping cleanup",
                            worker_id, feed_id
                        );
                        w.metrics.record_task_processed(
                            "cleanup_feed",
                            "skipped_in_progress",
                            task_start.elapsed().as_secs_f64(),
                        );
                        w.metrics.dec_workers_busy();
                        continue;
                    }
                };

                let cleanup_start = Instant::now();
                let mut outcome = "ok";
                if let Ok(conn) = w.pool.get() {
                    match crate::db::retention::cleanup_feed(&conn, feed_id) {
                        Ok(0) => {}
                        Ok(n) => {
                            info!(
                                "Retention cleanup deleted {} old entries for feed {}",
                                n, feed_id
                            );
                            w.metrics.record_retention_cleanup(
                                "feed",
                                cleanup_start.elapsed().as_secs_f64(),
                                n as u64,
                            );
                        }
                        Err(e) => {
                            warn!("Retention cleanup failed for feed {}: {:?}", feed_id, e);
                            outcome = "error";
                        }
                    }
                } else {
                    outcome = "error";
                }

                w.metrics.record_task_processed(
                    "cleanup_feed",
                    outcome,
                    task_start.elapsed().as_secs_f64(),
                );
            }

            TaskManagerCommand::CleanupAll => {
                let cleanup_start = Instant::now();
                let mut outcome = "ok";
                if let Ok(conn) = w.pool.get() {
                    match crate::db::retention::cleanup_all(&conn) {
                        Ok(0) => {}
                        Ok(n) => {
                            info!("Retention cleanup deleted {} entries", n);
                            w.metrics.record_retention_cleanup(
                                "all",
                                cleanup_start.elapsed().as_secs_f64(),
                                n as u64,
                            );
                        }
                        Err(e) => {
                            warn!("Retention cleanup failed: {:?}", e);
                            outcome = "error";
                        }
                    }
                } else {
                    outcome = "error";
                }

                w.metrics.record_task_processed(
                    "cleanup_all",
                    outcome,
                    task_start.elapsed().as_secs_f64(),
                );
            }

            TaskManagerCommand::CacheEntryAssets { entry_id } => {
                let outcome =
                    match cache_entry_assets(&client, &w.pool, &w.data_dir, entry_id).await {
                        Ok(()) => "ok",
                        Err(e) => {
                            warn!("failed caching assets for entry {}: {:?}", entry_id, e);
                            "error"
                        }
                    };
                w.metrics.record_task_processed(
                    "cache_entry_assets",
                    outcome,
                    task_start.elapsed().as_secs_f64(),
                );
            }

            TaskManagerCommand::OptimizeFts => {
                run_maintenance(
                    &w.pool,
                    crate::db::task_queue::TASK_FTS_OPTIMIZE,
                    "FTS5 optimize",
                    |conn| {
                        conn.execute(
                            "INSERT INTO entries_fts(entries_fts) VALUES ('optimize')",
                            [],
                        )?;
                        Ok(())
                    },
                );
                w.metrics.record_task_processed(
                    "optimize_fts",
                    "ok",
                    task_start.elapsed().as_secs_f64(),
                );
            }

            TaskManagerCommand::WalCheckpointAnalyze => {
                run_maintenance(
                    &w.pool,
                    crate::db::task_queue::TASK_WAL_CHECKPOINT_ANALYZE,
                    "WAL checkpoint and ANALYZE",
                    |conn| {
                        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); ANALYZE;")?;
                        Ok(())
                    },
                );
                w.metrics.record_task_processed(
                    "wal_checkpoint_analyze",
                    "ok",
                    task_start.elapsed().as_secs_f64(),
                );
            }

            TaskManagerCommand::IncrementalVacuum => {
                run_maintenance(
                    &w.pool,
                    crate::db::task_queue::TASK_INCREMENTAL_VACUUM,
                    "incremental vacuum",
                    |conn| {
                        conn.execute_batch("PRAGMA incremental_vacuum;")?;
                        Ok(())
                    },
                );
                w.metrics.record_task_processed(
                    "incremental_vacuum",
                    "ok",
                    task_start.elapsed().as_secs_f64(),
                );
            }
        }

        w.metrics.dec_workers_busy();
    }
}
