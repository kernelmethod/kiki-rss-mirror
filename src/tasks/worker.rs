use crate::config::ConfigHandle;
use crate::db::Db;
use crate::fetcher::Fetcher;
use crate::metrics::Metrics;
use crate::scripting::{ScriptRunner, ScriptRunnerHandle};
use crate::tasks::command::TaskManagerCommand;
use crate::tasks::entry_assets::cache_entry_assets;
use crate::tasks::error::FetchError;
use crate::tasks::error_recording::set_feed_error;
use crate::tasks::favicons::cache_feed_favicon;
use crate::tasks::fetch::refresh_feed;
use crate::tasks::maintenance::run_maintenance;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::Semaphore;
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

/// Shared state for a pool of workers that process [`TaskManagerCommand`]s.
///
/// Workers pull commands from a shared channel and dispatch events through a
/// server-wide [`ScriptRunnerHandle`]. Separate in-progress sets prevent two workers
/// from refreshing (or cleaning up) the same feed simultaneously.
#[derive(Clone)]
struct Worker {
    rx: crate::tasks::TaskReceiver,
    tx: crate::tasks::TaskSender,
    db: Db,
    token: CancellationToken,
    refresh_in_progress: InProgressSet,
    metrics: Arc<Metrics>,
    data_dir: PathBuf,
    config: ConfigHandle,
    script_runner: ScriptRunnerHandle,
    /// Retrieves feeds and downloads their assets.
    fetcher: Fetcher,
    /// Taken by a refresh to store what it fetched; see
    /// [`STORE_CONCURRENCY`].
    store_permits: Arc<Semaphore>,
}

/// The fewest worker tasks [`worker_count`] chooses, however few CPUs
/// there are.
pub const MIN_WORKERS: usize = 16;

/// Worker tasks [`worker_count`] chooses for each CPU.
pub const WORKERS_PER_CPU: usize = 4;

/// The most feed refreshes that store what they fetched at the same time;
/// the rest wait their turn once their fetch is done. See [`worker_count`].
pub const STORE_CONCURRENCY: usize = 2;

/// Determine the number of worker tasks to spawn: [`WORKERS_PER_CPU`] for
/// each CPU, and at least [`MIN_WORKERS`].
///
/// A worker spends nearly all of a refresh waiting on the feed's server,
/// not on a CPU, so sizing the pool by CPUs alone left a few slow servers
/// holding every worker while the queue filled up behind them. The work a
/// refresh does do here, storing what it fetched, goes through the one
/// writer connection, so only [`STORE_CONCURRENCY`] refreshes store at
/// once, however many workers there are.
///
/// # Examples
///
/// ```
/// use kiki_rss::tasks::{worker_count, MIN_WORKERS};
///
/// assert!(worker_count() >= MIN_WORKERS);
/// ```
pub fn worker_count() -> usize {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    cpus.saturating_mul(WORKERS_PER_CPU).max(MIN_WORKERS)
}

/// Spawn multiple worker tasks that pull from a shared queue.
///
/// `rx` is a [`crate::tasks::TaskReceiver`], or a plain receiver for a
/// queue with a single lane.
///
/// All workers share a single [`ScriptRunnerHandle`], installed when the server starts.
/// They also share one [`Fetcher`], through which every feed refresh
/// retrieves and parses its feed and every asset is downloaded, and read
/// settings from `config` afresh for every command, so a settings change
/// applies to the next one.
#[allow(clippy::too_many_arguments)]
pub fn spawn_workers(
    rx: impl Into<crate::tasks::TaskReceiver>,
    tx: crate::tasks::TaskSender,
    db: Db,
    token: CancellationToken,
    num_workers: usize,
    metrics: Arc<Metrics>,
    data_dir: PathBuf,
    config: ConfigHandle,
    script_runner: ScriptRunnerHandle,
    fetcher: Fetcher,
) -> Vec<tokio::task::JoinHandle<()>> {
    let worker = Worker {
        rx: rx.into(),
        tx,
        db,
        token,
        refresh_in_progress: Arc::new(Mutex::new(HashSet::new())),
        metrics,
        data_dir,
        config,
        script_runner,
        fetcher,
        store_permits: Arc::new(Semaphore::new(STORE_CONCURRENCY)),
    };
    let mut handles = Vec::with_capacity(num_workers);

    for worker_id in 0..num_workers {
        let worker = worker.clone();

        handles.push(tokio::spawn(run_worker(worker_id, worker)));
    }

    handles
}

/// A single worker loop that pulls commands from the shared channel.
///
/// Returns when the channel closes or `w.token` is cancelled.
async fn run_worker(worker_id: usize, w: Worker) {
    loop {
        let command = tokio::select! {
            cmd = w.rx.recv() => {
                match cmd {
                    Ok(c) => c,
                    Err(_) => return, // channel closed
                }
            }
            _ = w.token.cancelled() => return,
        };
        // Off the queue: the same refresh may be queued again from now on.
        w.tx.dequeued(&command);

        w.metrics.inc_workers_busy();
        let task_start = Instant::now();

        // Each command runs in a task of its own, so that a panic while
        // handling it unwinds that task alone rather than this loop: the
        // worker lives on to take the next command.
        let handled = tokio::spawn({
            let w = w.clone();
            let command = command.clone();
            async move { handle_command(worker_id, &w, command, task_start).await }
        })
        .await;
        if let Err(e) = handled {
            recover_from_failed_command(&w, command, task_start, e);
        }

        w.metrics.dec_workers_busy();
    }
}

/// Deal with a command whose task did not finish: log it, count it, and,
/// for a feed refresh, record the failure against the feed.
///
/// Recording it is what stops one bad feed from taking the workers down
/// one after another: the refresh may have panicked before it rescheduled
/// the feed, which would leave it due again on the scheduler's next tick,
/// to panic in the next worker. As a transient error it backs off instead.
fn recover_from_failed_command(
    w: &Worker,
    command: TaskManagerCommand,
    task_start: Instant,
    e: tokio::task::JoinError,
) {
    let what = if e.is_panic() {
        format!("panicked: {}", panic_message(e.into_panic()))
    } else {
        "was cancelled".to_string()
    };
    error!("task {:?} {}", command, what);
    w.metrics
        .record_task_processed(command.name(), "panic", task_start.elapsed().as_secs_f64());

    if let TaskManagerCommand::RefreshFeed { feed_id, .. } = command {
        let settings = w.config.current();
        let schedule = w.db.write_blocking(|conn| {
            set_feed_error(
                conn,
                feed_id,
                &FetchError::Other {
                    message: format!("refreshing the feed {}", what),
                },
                &settings.feed_fetch,
                &w.metrics,
            )
        });
        match schedule {
            Ok(Some(schedule)) => info!("Feed {}: next attempt {}", feed_id, schedule),
            Ok(None) => {}
            Err(e) => error!("could not record the failure of feed {}: {:?}", feed_id, e),
        }
    }
}

/// The message a panic was raised with, if it was a string.
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(s) => *s,
        Err(payload) => match payload.downcast::<&'static str>() {
            Ok(s) => (*s).to_string(),
            Err(_) => "(no message)".to_string(),
        },
    }
}

/// Carry out one command. Runs in a task of its own; see [`run_worker`].
async fn handle_command(
    worker_id: usize,
    w: &Worker,
    command: TaskManagerCommand,
    task_start: Instant,
) {
    let settings = w.config.current();

    match command {
        TaskManagerCommand::RefreshFeed { feed_id, manual } => {
            let guard = match InProgressGuard::try_claim(&w.refresh_in_progress, feed_id) {
                Some(g) => g,
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
                    return;
                }
            };

            let runner_snapshot = w.script_runner.current();
            let script_runner: Option<&dyn ScriptRunner> = runner_snapshot.as_deref();

            let outcome = match refresh_feed(
                &w.fetcher,
                feed_id,
                manual,
                w.db.clone(),
                &settings,
                script_runner,
                &w.metrics,
                &w.tx,
                &w.store_permits,
            )
            .await
            {
                Ok(()) => "ok",
                Err(e) => {
                    error!(
                        "An error occurred while refreshing feed {}: {:?}",
                        feed_id, e
                    );
                    let schedule = w.db.write_blocking(|conn| {
                        set_feed_error(
                            conn,
                            feed_id,
                            &FetchError::Other {
                                message: format!("{}", e),
                            },
                            &settings.feed_fetch,
                            &w.metrics,
                        )
                    });
                    if let Ok(Some(schedule)) = schedule {
                        info!("Feed {}: next attempt {}", feed_id, schedule);
                    }
                    "error"
                }
            };

            drop(guard);

            w.metrics.record_task_processed(
                "refresh_feed",
                outcome,
                task_start.elapsed().as_secs_f64(),
            );
        }

        TaskManagerCommand::CacheEntryAssets { entry_id } => {
            let result = cache_entry_assets(
                &w.fetcher,
                &settings.effective_proxy(),
                &w.db,
                &w.data_dir,
                &settings.asset_cache,
                entry_id,
            )
            .await;
            let outcome = match result {
                Ok(()) => {
                    // Otherwise the entry is queued again once it falls
                    // due; see `crate::db::pending_assets`.
                    let done =
                        w.db.write_blocking(|conn| crate::db::pending_assets::done(conn, entry_id))
                            .map_err(anyhow::Error::from)
                            .and_then(|r| r.map_err(anyhow::Error::from));
                    if let Err(e) = done {
                        warn!(
                            "could not mark the assets of entry {} cached: {:?}",
                            entry_id, e
                        );
                    }
                    "ok"
                }
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

        TaskManagerCommand::CacheFeedFavicon { feed_id } => {
            let result = cache_feed_favicon(
                &w.fetcher,
                &settings.effective_proxy(),
                &w.db,
                &w.data_dir,
                &settings.asset_cache,
                feed_id,
            )
            .await;
            let outcome = match result {
                Ok(()) => "ok",
                Err(e) => {
                    warn!("failed caching the favicon of feed {}: {:?}", feed_id, e);
                    "error"
                }
            };
            w.metrics.record_task_processed(
                "cache_feed_favicon",
                outcome,
                task_start.elapsed().as_secs_f64(),
            );
        }

        TaskManagerCommand::OptimizeFts => {
            run_maintenance(
                &w.db,
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
                &w.db,
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
                &w.db,
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
        TaskManagerCommand::IntegrityCheck => {
            let outcome = match crate::db::integrity::quick_check(&w.db) {
                Ok(problems) if problems.is_empty() => {
                    info!("database integrity check found no problems");
                    w.metrics.set_db_integrity_ok(true);
                    "ok"
                }
                Ok(problems) => {
                    error!(
                        "database integrity check found problems; restore a backup, or \
                         run `sqlite3 kiki.db .recover` on a copy: {}",
                        problems.join("; ")
                    );
                    w.metrics.set_db_integrity_ok(false);
                    "error"
                }
                Err(e) => {
                    warn!("database integrity check could not run: {:?}", e);
                    "error"
                }
            };
            // Counted as run either way: a failed check is reported, not
            // retried in a loop.
            let recorded = w.db.write_blocking(|conn| {
                crate::db::task_queue::record_run(conn, crate::db::task_queue::TASK_INTEGRITY_CHECK)
            });
            match recorded {
                Ok(Ok(())) => {}
                Ok(Err(e)) => warn!("Failed to record integrity check run: {:?}", e),
                Err(e) => warn!("Failed to record integrity check run: {:?}", e),
            }
            w.metrics.record_task_processed(
                "integrity_check",
                outcome,
                task_start.elapsed().as_secs_f64(),
            );
        }
    }
}
