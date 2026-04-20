use crate::http::USER_AGENT;
use crate::metrics::Metrics;
use crate::scripting::{FeedEntry, ScriptRunner};
use crate::tasks::cache::{corrected_max_age, extract_server_hints, parse_http_date, CacheControl};
use anyhow::Result;
use chrono::{DateTime, Utc};
use r2d2::{Pool, PooledConnection};
use r2d2_sqlite::SqliteConnectionManager;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

pub mod assets;
mod cache;

#[cfg(test)]
mod tests;

/// Structured representation of feed fetch errors.
///
/// Serialized to JSON for storage in the database `last_fetch_error` column
/// and included in API responses.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(tag = "type")]
pub enum FetchError {
    /// The response body could not be parsed as RSS or Atom.
    #[serde(rename = "invalid_feed")]
    InvalidFeed { url: String },
    /// The server returned a non-success HTTP status code.
    #[serde(rename = "http_status")]
    HttpStatus { url: String, status: u16 },
    /// The maximum number of redirects was exceeded.
    #[serde(rename = "too_many_redirects")]
    TooManyRedirects { url: String },
    /// A network or other unexpected error occurred.
    #[serde(rename = "other")]
    Other { message: String },
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::InvalidFeed { url } => {
                write!(
                    f,
                    "Response from {} was not detected as a valid RSS or Atom feed",
                    url
                )
            }
            FetchError::HttpStatus { url, status } => {
                write!(f, "Received HTTP status {} while fetching {}", status, url)
            }
            FetchError::TooManyRedirects { url } => {
                write!(f, "Exceeded maximum redirects while fetching {}", url)
            }
            FetchError::Other { message } => write!(f, "{}", message),
        }
    }
}

impl FetchError {
    /// Classify a fetch failure as transient (worth retrying with backoff) or
    /// permanent (wait the full backoff cap before the next attempt).
    ///
    /// Transient: 408 Request Timeout, 429 Too Many Requests, any 5xx,
    /// network/timeout errors (`Other`).
    /// Permanent: other 4xx statuses, malformed feed bodies, redirect loops.
    pub fn is_transient(&self) -> bool {
        match self {
            FetchError::HttpStatus { status, .. } => {
                matches!(*status, 408 | 429) || (500..=599).contains(status)
            }
            FetchError::Other { .. } => true,
            FetchError::InvalidFeed { .. } | FetchError::TooManyRedirects { .. } => false,
        }
    }
}

/// Parse an HTTP `Retry-After` header value into an absolute Unix timestamp.
///
/// RFC 7231 §7.1.3 allows either a non-negative integer delta-seconds or an
/// HTTP-date. Returns `None` if the value cannot be parsed.
fn parse_retry_after(value: &str, now: DateTime<Utc>) -> Option<i64> {
    let trimmed = value.trim();

    if let Ok(secs) = trimmed.parse::<u64>() {
        return Some(now.timestamp().saturating_add(secs as i64));
    }

    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(trimmed, "%a, %d %b %Y %H:%M:%S GMT") {
        return Some(dt.and_utc().timestamp());
    }

    if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(trimmed) {
        return Some(dt.timestamp());
    }

    None
}

/// Outcome of a fetch attempt, used to compute the next eligibility time.
#[derive(Debug, Clone, Copy)]
enum FetchOutcome {
    /// Fresh 200 OK or a file:// read.
    Success { server_hint_secs: Option<u64> },
    /// 304 Not Modified.
    NotModified { server_hint_secs: Option<u64> },
    /// Transient error — eligible for exponential backoff.
    ///
    /// `stale_if_error_secs` carries the most recent
    /// `Cache-Control: stale-if-error` value (RFC 5861 §4). When present, it
    /// caps the computed backoff so we revalidate before the grace window
    /// elapses.
    TransientErr {
        retry_after_ts: Option<i64>,
        consecutive_failures: u32,
        stale_if_error_secs: Option<u64>,
    },
    /// Permanent error — wait the full backoff cap.
    PermanentErr,
}

/// Compute the Unix timestamp at which a feed is next eligible to be fetched.
///
/// Rules (all results clamped to `[now + min_cadence, now + max_backoff]`):
/// - `Success` / `NotModified`: use `min(server_hint, min_fetch_interval)`
///   when a hint is present; otherwise use `min_fetch_interval`. The per-feed
///   `min_fetch_interval` is a ceiling, so we never wait longer than it.
/// - `TransientErr` with `Retry-After`: honor the server's deadline.
/// - `TransientErr` without `Retry-After`: exponential backoff
///   `min_cadence * 2^(consecutive_failures - 1)`.
/// - `PermanentErr`: wait the full `max_backoff`.
fn compute_next_fetch_at(
    outcome: FetchOutcome,
    now_ts: i64,
    min_cadence: u64,
    max_backoff: u64,
    min_fetch_interval: u64,
) -> i64 {
    let min_cadence = min_cadence.max(1);
    let max_backoff = max_backoff.max(min_cadence);

    let raw_next_ts: i64 = match outcome {
        FetchOutcome::Success { server_hint_secs }
        | FetchOutcome::NotModified { server_hint_secs } => {
            let interval = match server_hint_secs {
                Some(hint) => hint.min(min_fetch_interval),
                None => min_fetch_interval,
            };
            now_ts.saturating_add(interval as i64)
        }
        FetchOutcome::TransientErr {
            retry_after_ts: Some(ts),
            stale_if_error_secs,
            ..
        } => {
            // Don't wait past the stale-if-error window (RFC 5861 §4).
            match stale_if_error_secs {
                Some(s) => ts.min(now_ts.saturating_add(s as i64)),
                None => ts,
            }
        }
        FetchOutcome::TransientErr {
            retry_after_ts: None,
            consecutive_failures,
            stale_if_error_secs,
        } => {
            let shift = consecutive_failures.saturating_sub(1).min(63);
            let multiplier = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
            let mut backoff = min_cadence.saturating_mul(multiplier);
            if let Some(s) = stale_if_error_secs {
                backoff = backoff.min(s);
            }
            now_ts.saturating_add(backoff as i64)
        }
        FetchOutcome::PermanentErr => now_ts.saturating_add(max_backoff as i64),
    };

    let floor = now_ts.saturating_add(min_cadence as i64);
    let ceiling = now_ts.saturating_add(max_backoff as i64);
    raw_next_ts.max(floor).min(ceiling)
}

#[derive(Debug, Clone)]
pub enum TaskManagerCommand {
    RefreshFeed(i64),
    /// Run retention cleanup for the given feed.
    CleanupFeed(i64),
    /// Run retention cleanup across all feeds.
    CleanupAll,
    /// Download and cache the external assets referenced by an entry
    /// (inline images plus any enclosure).
    CacheEntryAssets {
        entry_id: i64,
    },
    /// Merge FTS5 index segments to improve search performance.
    OptimizeFts,
    /// Checkpoint the WAL file and refresh query-planner statistics.
    WalCheckpointAnalyze,
    /// Reclaim free pages via `PRAGMA incremental_vacuum`.
    IncrementalVacuum,
}

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
/// Each worker pulls commands from a shared channel and maintains its own
/// script runner. Separate in-progress sets prevent two workers from
/// refreshing (or cleaning up) the same feed simultaneously.
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
}

/// Determine the number of worker tasks to spawn.
pub fn worker_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

/// Spawn multiple worker tasks that pull from a shared channel.
///
/// Each worker maintains its own `LuaScriptRunner` (when the `lua` feature is
/// enabled) and subscribes to a `watch` channel for reload signals.
#[allow(clippy::too_many_arguments)]
pub fn spawn_workers(
    rx: async_channel::Receiver<TaskManagerCommand>,
    tx: async_channel::Sender<TaskManagerCommand>,
    pool: Pool<SqliteConnectionManager>,
    token: CancellationToken,
    reload_tx: tokio::sync::watch::Sender<()>,
    num_workers: usize,
    metrics: Arc<Metrics>,
    data_dir: PathBuf,
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
    };
    let mut handles = Vec::with_capacity(num_workers);

    for worker_id in 0..num_workers {
        let worker = worker.clone();
        let reload_rx = reload_tx.subscribe();

        handles.push(tokio::spawn(run_worker(worker_id, worker, reload_rx)));
    }

    handles
}

/// A single worker loop that pulls commands from the shared channel.
async fn run_worker(
    worker_id: usize,
    w: Worker,
    mut reload_rx: tokio::sync::watch::Receiver<()>,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(USER_AGENT)
        .build()?;

    // Each worker has its own script runner, built eagerly from the current
    // database state.  Rebuilt when a reload signal arrives via the watch channel.
    #[cfg(feature = "lua")]
    let mut runner: Option<crate::scripting::lua::LuaScriptRunner> =
        build_runner(&w.pool, &w.metrics);

    loop {
        // Check for a pending reload signal before processing the next command.
        #[cfg(feature = "lua")]
        if reload_rx.has_changed().unwrap_or(false) {
            // Mark the current value as seen so has_changed() returns false
            // until the next send.
            reload_rx.borrow_and_update();
            debug!(
                "Worker {} reloading LuaScriptRunner from database",
                worker_id
            );
            runner = build_runner(&w.pool, &w.metrics);
            info!("Worker {} LuaScriptRunner reloaded", worker_id);
        }

        let command = tokio::select! {
            cmd = w.rx.recv() => {
                match cmd {
                    Ok(c) => c,
                    Err(_) => return Ok(()), // channel closed
                }
            }
            _ = w.token.cancelled() => return Ok(()),
            _ = reload_rx.changed() => {
                // A reload signal arrived while we were waiting for a command.
                #[cfg(feature = "lua")]
                {
                    debug!("Worker {} reloading LuaScriptRunner from database", worker_id);
                    runner = build_runner(&w.pool, &w.metrics);
                    info!("Worker {} LuaScriptRunner reloaded", worker_id);
                }
                continue;
            }
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

                #[cfg(feature = "lua")]
                let script_runner: Option<&dyn ScriptRunner> =
                    runner.as_ref().map(|r| r as &dyn ScriptRunner);
                #[cfg(not(feature = "lua"))]
                let script_runner: Option<&dyn ScriptRunner> = None;

                let outcome = match refresh_feed(
                    &client,
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

/// Run a maintenance operation and, on success, update its `task_queue`
/// row so the schedule survives server restarts.
pub(crate) fn run_maintenance<F>(
    pool: &Pool<SqliteConnectionManager>,
    task_type: &str,
    label: &str,
    op: F,
) where
    F: FnOnce(&PooledConnection<SqliteConnectionManager>) -> Result<()>,
{
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            warn!(
                "{} skipped: failed to acquire DB connection: {:?}",
                label, e
            );
            return;
        }
    };
    match op(&conn) {
        Ok(()) => {
            info!("{} completed", label);
            if let Err(e) = crate::db::task_queue::record_run(&conn, task_type) {
                warn!("Failed to record {} run: {:?}", label, e);
            }
        }
        Err(e) => warn!("{} failed: {:?}", label, e),
    }
}

/// Load all Lua script source texts from the database.
#[cfg(feature = "lua")]
fn load_all_script_sources(
    conn: &r2d2::PooledConnection<SqliteConnectionManager>,
) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT text FROM scripts ORDER BY id")?;
    let rows = stmt.query_map([], |row| row.get(0))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Build a [`LuaScriptRunner`] from all scripts currently in the database.
///
/// Returns `None` and logs a warning if the runner cannot be constructed.
#[cfg(feature = "lua")]
fn build_runner(
    pool: &Pool<SqliteConnectionManager>,
    metrics: &Metrics,
) -> Option<crate::scripting::lua::LuaScriptRunner> {
    match pool.get() {
        Ok(conn) => match load_all_script_sources(&conn) {
            Ok(sources) => {
                let count = sources.len() as f64;
                match crate::scripting::lua::LuaScriptRunner::new(&sources) {
                    Ok(r) => {
                        metrics.set_scripts_loaded(count);
                        Some(r)
                    }
                    Err(e) => {
                        warn!("failed to compile Lua scripts: {}", e);
                        metrics.record_script_compile_error();
                        metrics.set_scripts_loaded(0.0);
                        None
                    }
                }
            }
            Err(e) => {
                error!("failed to load script sources from database: {}", e);
                None
            }
        },
        Err(e) => {
            error!(
                "failed to get DB connection while building script runner: {}",
                e
            );
            None
        }
    }
}

/// Record a fetch error for a feed in the database and reschedule the next
/// fetch attempt.
///
/// `retry_after_ts` is the absolute Unix timestamp parsed from the
/// server-provided `Retry-After` header (if any). It takes precedence over
/// computed exponential backoff for transient errors.
///
/// The `consecutive_failures` column is incremented atomically; the rescheduled
/// `next_fetch_at` is computed from that new streak length.
#[allow(clippy::too_many_arguments)]
fn set_feed_error_with_schedule(
    conn: &PooledConnection<SqliteConnectionManager>,
    feed_id: i64,
    fetch_error: &FetchError,
    retry_after_ts: Option<i64>,
    stale_if_error_secs: Option<u64>,
    min_cadence: u64,
    max_backoff: u64,
    min_fetch_interval: u64,
    metrics: &Metrics,
) {
    let json = match serde_json::to_string(fetch_error) {
        Ok(j) => j,
        Err(e) => {
            error!(
                "Failed to serialize fetch error for feed {}: {:?}",
                feed_id, e
            );
            return;
        }
    };

    let now_ts = Utc::now().timestamp();
    let is_transient = fetch_error.is_transient();

    // Read the current failure count so we can compute the new streak length
    // without a race window. Default to 0 if the row is somehow missing.
    let current_failures: u32 = conn
        .query_row(
            "SELECT consecutive_failures FROM feeds WHERE id = ?1",
            [feed_id],
            |row| row.get::<_, i64>(0),
        )
        .map(|v| v.max(0) as u32)
        .unwrap_or(0);
    let new_failures = current_failures.saturating_add(1);

    let (outcome, retry_kind): (FetchOutcome, &'static str) = if is_transient {
        let retry_kind = if retry_after_ts.is_some() {
            "retry_after"
        } else {
            "backoff"
        };
        (
            FetchOutcome::TransientErr {
                retry_after_ts,
                consecutive_failures: new_failures,
                stale_if_error_secs,
            },
            retry_kind,
        )
    } else {
        (FetchOutcome::PermanentErr, "permanent")
    };

    let next_fetch_at = compute_next_fetch_at(
        outcome,
        now_ts,
        min_cadence,
        max_backoff,
        min_fetch_interval,
    );

    if let Err(e) = conn.execute(
        "UPDATE feeds SET
            last_fetch_error = ?1,
            last_fetch_error_at = ?2,
            retry_after_at = ?3,
            consecutive_failures = consecutive_failures + 1,
            next_fetch_at = ?4
         WHERE id = ?5",
        (&json, now_ts, retry_after_ts, next_fetch_at, feed_id),
    ) {
        error!(
            "Failed to persist fetch error for feed {}: {:?}",
            feed_id, e
        );
        return;
    }

    metrics.record_feed_retry_scheduled(retry_kind, (next_fetch_at - now_ts) as f64);
    metrics.record_feed_consecutive_failures(new_failures);
}

/// Back-compat wrapper around [`set_feed_error_with_schedule`] for callsites
/// that don't have a `Retry-After` timestamp and need to read the tuning
/// settings themselves. Looks them up from the database.
fn set_feed_error(
    conn: &PooledConnection<SqliteConnectionManager>,
    feed_id: i64,
    fetch_error: &FetchError,
    metrics: &Metrics,
) {
    let (min_cadence, max_backoff) = match (
        crate::db::settings::get_min_polling_cadence_seconds(conn),
        crate::db::settings::get_max_feed_backoff_seconds(conn),
    ) {
        (Ok(c), Ok(b)) => (c, b),
        _ => {
            error!(
                "Failed to read scheduler settings while recording error for feed {}",
                feed_id
            );
            return;
        }
    };
    let min_fetch_interval = conn
        .query_row(
            "SELECT min_fetch_interval_seconds FROM feeds WHERE id = ?1",
            [feed_id],
            |row| row.get::<_, i64>(0),
        )
        .map(|v| v.max(0) as u64)
        .unwrap_or(10800);

    set_feed_error_with_schedule(
        conn,
        feed_id,
        fetch_error,
        None,
        None,
        min_cadence,
        max_backoff,
        min_fetch_interval,
        metrics,
    );
}

/// Clear any previously recorded fetch error for a feed and reset the failure
/// streak counter.
fn clear_feed_error(conn: &PooledConnection<SqliteConnectionManager>, feed_id: i64) {
    if let Err(e) = conn.execute(
        "UPDATE feeds SET
            last_fetch_error = NULL,
            last_fetch_error_at = NULL,
            retry_after_at = NULL,
            consecutive_failures = 0
         WHERE id = ?1",
        (feed_id,),
    ) {
        error!("Failed to clear fetch error for feed {}: {:?}", feed_id, e);
    }
}

/// Settings controlling the fetch scheduler, read together so `refresh_feed`
/// and its helpers don't hit the database separately.
#[derive(Debug, Clone, Copy)]
struct SchedulerConfig {
    min_cadence: u64,
    max_backoff: u64,
    min_fetch_interval: u64,
}

/// Refresh the feed corresponding to the provided `feed_id`.
pub(crate) async fn refresh_feed(
    client: &reqwest::Client,
    feed_id: i64,
    pool: Pool<SqliteConnectionManager>,
    script_runner: Option<&dyn ScriptRunner>,
    metrics: &Metrics,
    task_tx: &async_channel::Sender<TaskManagerCommand>,
) -> Result<()> {
    let fetch_start = Instant::now();

    // Load the feed URL, conditional-request headers, scheduling state, and
    // per-feed override all at once.
    let conn = pool.get()?;
    let (
        feed_url,
        header_etag,
        header_last_modified,
        header_immutable_until,
        next_fetch_at,
        min_fetch_interval,
    ): (
        String,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<i64>,
        i64,
    ) = conn.query_row(
        "SELECT url, header_etag, header_last_modified, header_immutable_until, next_fetch_at, min_fetch_interval_seconds FROM feeds WHERE id = ?1",
        [feed_id],
        |row| {
            let url: String = row.get(0)?;
            let etag: Option<String> = row.get(1)?;
            let last_modified: Option<String> = row.get(2)?;
            let immutable_until: Option<i64> = row.get(3)?;
            let next_fetch_at: Option<i64> = row.get(4)?;
            let min_fetch_interval: i64 = row.get(5)?;
            Ok((url, etag, last_modified, immutable_until, next_fetch_at, min_fetch_interval))
        },
    )?;

    let cfg = SchedulerConfig {
        min_cadence: crate::db::settings::get_min_polling_cadence_seconds(&conn)?,
        max_backoff: crate::db::settings::get_max_feed_backoff_seconds(&conn)?,
        min_fetch_interval: min_fetch_interval.max(0) as u64,
    };

    // Eligibility gate: a scheduled `next_fetch_at` in the future means skip.
    if let Some(next_ts) = next_fetch_at {
        let now_ts = Utc::now().timestamp();
        if now_ts < next_ts {
            debug!(
                "Feed {} not yet eligible; next fetch in {} seconds",
                feed_id,
                next_ts - now_ts
            );
            metrics.record_feed_cache_hit("next_fetch_at");
            metrics.record_feed_fetch("cache_hit", fetch_start.elapsed().as_secs_f64());
            return Ok(());
        }
    }

    // Handle file:// URLs differently
    let feed_content = if feed_url.starts_with("file://") {
        retrieve_file_feed(&feed_url, feed_id, pool.clone(), cfg)
    } else {
        retrieve_feed(
            client,
            feed_id,
            &feed_url,
            header_etag.as_deref(),
            header_last_modified.as_deref(),
            header_immutable_until,
            pool.clone(),
            fetch_start,
            cfg,
            metrics,
        )
        .await
    };

    let feed_content = match feed_content {
        Ok(c) => c,
        Err(e) => {
            metrics.record_feed_fetch("other", fetch_start.elapsed().as_secs_f64());
            return Err(e);
        }
    };

    let content = if let Some(content) = feed_content {
        content
    } else {
        // retrieve_feed / retrieve_file_feed already recorded the outcome.
        return Ok(());
    };

    // Attempt to parse content as Atom, with fallback to RSS.
    let parse_start = Instant::now();
    if let Ok(feed) = atom_syndication::Feed::read_from(&content[..]) {
        let entries = feed.entries.len() as u64;
        metrics.record_feed_parse("atom", parse_start.elapsed().as_secs_f64(), entries);
        let inserted = process_atom_feed(feed_id, feed, conn, script_runner, metrics)?;
        enqueue_asset_caching(task_tx, metrics, &inserted);
        clear_feed_error(&pool.get()?, feed_id);
        metrics.record_feed_fetch("success", fetch_start.elapsed().as_secs_f64());
    } else if let Ok(channel) = rss::Channel::read_from(&content[..]) {
        let items = channel.items.len() as u64;
        metrics.record_feed_parse("rss", parse_start.elapsed().as_secs_f64(), items);
        let inserted = process_rss_feed(feed_id, channel, conn, script_runner, metrics)?;
        enqueue_asset_caching(task_tx, metrics, &inserted);
        clear_feed_error(&pool.get()?, feed_id);
        metrics.record_feed_fetch("success", fetch_start.elapsed().as_secs_f64());
    } else {
        let fetch_err = FetchError::InvalidFeed {
            url: feed_url.clone(),
        };
        warn!("Feed {}: {}", feed_id, fetch_err);
        set_feed_error_with_schedule(
            &pool.get()?,
            feed_id,
            &fetch_err,
            None,
            None,
            cfg.min_cadence,
            cfg.max_backoff,
            cfg.min_fetch_interval,
            metrics,
        );
        metrics.record_feed_fetch("invalid_feed", fetch_start.elapsed().as_secs_f64());
        return Ok(());
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn retrieve_feed(
    client: &reqwest::Client,
    feed_id: i64,
    feed_url: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
    immutable_until: Option<i64>,
    pool: Pool<SqliteConnectionManager>,
    fetch_start: Instant,
    cfg: SchedulerConfig,
    metrics: &Metrics,
) -> Result<Option<Vec<u8>>> {
    let conn = pool.get()?;

    let timeout = Duration::from_secs(crate::db::settings::get_feed_update_timeout_seconds(&conn)?);

    // RFC 8246: while a prior response advertised `immutable` and is still
    // fresh, suppress conditional revalidation — the server has promised
    // the representation won't change.
    let skip_conditionals = immutable_until
        .map(|until| Utc::now().timestamp() < until)
        .unwrap_or(false);

    let mut current_url = feed_url.to_string();
    let mut had_permanent_redirect = false;
    let mut redirects: u64 = 0;
    let max_redirects = 10;

    let resp = 'redirect: {
        for _ in 0..=max_redirects {
            // Only send conditional headers on the first request
            let mut request = client.get(&current_url).timeout(timeout);
            if current_url == feed_url && !skip_conditionals {
                if let Some(etag) = etag {
                    request = request.header("If-None-Match", etag);
                }
                if let Some(last_modified) = last_modified {
                    request = request.header("If-Modified-Since", last_modified);
                }
            }

            let resp = match request.send().await {
                Ok(r) => r,
                Err(e) => {
                    let outcome = if e.is_timeout() { "timeout" } else { "other" };
                    let fetch_err = FetchError::Other {
                        message: format!("{}", e),
                    };
                    warn!("Feed {}: {}", feed_id, fetch_err);
                    set_feed_error_with_schedule(
                        &conn,
                        feed_id,
                        &fetch_err,
                        None,
                        None,
                        cfg.min_cadence,
                        cfg.max_backoff,
                        cfg.min_fetch_interval,
                        metrics,
                    );
                    metrics.record_feed_fetch(outcome, fetch_start.elapsed().as_secs_f64());
                    metrics.record_feed_redirects(redirects);
                    return Ok(None);
                }
            };

            if resp.status().is_redirection() && resp.status() != reqwest::StatusCode::NOT_MODIFIED
            {
                let location = resp
                    .headers()
                    .get("location")
                    .and_then(|h| h.to_str().ok())
                    .ok_or_else(|| anyhow::anyhow!("Redirect response missing Location header"))?
                    .to_string();

                // 301 Moved Permanently and 308 Permanent Redirect both indicate a
                // permanent move
                if resp.status() == reqwest::StatusCode::MOVED_PERMANENTLY
                    || resp.status() == reqwest::StatusCode::PERMANENT_REDIRECT
                {
                    had_permanent_redirect = true;
                }

                // Resolve the Location against the current URL to handle relative redirects
                let base = Url::parse(&current_url)?;
                current_url = base.join(&location)?.to_string();
                redirects += 1;
                continue;
            }

            break 'redirect resp;
        }

        let fetch_err = FetchError::TooManyRedirects {
            url: feed_url.to_string(),
        };
        warn!("Feed {}: {}", feed_id, fetch_err);
        set_feed_error_with_schedule(
            &conn,
            feed_id,
            &fetch_err,
            None,
            None,
            cfg.min_cadence,
            cfg.max_backoff,
            cfg.min_fetch_interval,
            metrics,
        );
        metrics.record_feed_fetch("too_many_redirects", fetch_start.elapsed().as_secs_f64());
        metrics.record_feed_redirects(redirects);
        return Ok(None);
    };

    metrics.record_feed_redirects(redirects);

    // Check if the feed was modified
    match resp.status() {
        reqwest::StatusCode::NOT_MODIFIED => {
            info!("Feed {} was not modified since last check", feed_id);
            let now_ts = Utc::now().timestamp();
            let hints = extract_server_hints(resp.headers(), now_ts);
            let next_fetch_at = compute_next_fetch_at(
                FetchOutcome::NotModified {
                    server_hint_secs: hints.hint_secs,
                },
                now_ts,
                cfg.min_cadence,
                cfg.max_backoff,
                cfg.min_fetch_interval,
            );
            conn.execute(
                "UPDATE feeds SET
                    last_checked = ?1,
                    next_fetch_at = ?2,
                    consecutive_failures = 0,
                    retry_after_at = NULL
                 WHERE id = ?3",
                (now_ts, next_fetch_at, feed_id),
            )?;
            metrics.record_feed_cache_hit("not_modified");
            metrics.record_feed_retry_scheduled("cache_hint", (next_fetch_at - now_ts) as f64);
            metrics.record_feed_fetch("not_modified", fetch_start.elapsed().as_secs_f64());
            return Ok(None);
        }
        reqwest::StatusCode::OK => { /* Do nothing */ }
        // For other status codes, log an issue and stop processing
        status => {
            let status_u16 = status.as_u16();
            let fetch_err = FetchError::HttpStatus {
                url: feed_url.to_string(),
                status: status_u16,
            };
            warn!("Feed {}: {}", feed_id, fetch_err);

            // Parse Retry-After for 429 Too Many Requests and 503 Service
            // Unavailable. Other statuses don't carry retry hints.
            let retry_after_ts = if status_u16 == 429 || status_u16 == 503 {
                resp.headers()
                    .get("retry-after")
                    .and_then(|h| h.to_str().ok())
                    .and_then(|s| parse_retry_after(s, Utc::now()))
            } else {
                None
            };

            // Honor Cache-Control: stale-if-error on the error response
            // (RFC 5861 §4) so our backoff doesn't outlive the grace window.
            let hints = extract_server_hints(resp.headers(), Utc::now().timestamp());

            set_feed_error_with_schedule(
                &conn,
                feed_id,
                &fetch_err,
                retry_after_ts,
                hints.stale_if_error,
                cfg.min_cadence,
                cfg.max_backoff,
                cfg.min_fetch_interval,
                metrics,
            );
            metrics.record_feed_fetch("http_error", fetch_start.elapsed().as_secs_f64());
            return Ok(None);
        }
    }

    // If we followed a permanent redirect, update the stored URL in the database
    if had_permanent_redirect && current_url != feed_url {
        info!(
            "Feed {} permanently redirected from {} to {}; updating stored URL",
            feed_id, feed_url, current_url
        );
        conn.execute(
            "UPDATE feeds SET url = ?1 WHERE id = ?2",
            (&current_url, feed_id),
        )?;
    }

    // Update the feed's headers in the database
    let mut etag: Option<&str> = resp.headers().get("etag").and_then(|h| h.to_str().ok());
    let mut last_modified: Option<&str> = resp
        .headers()
        .get("last-modified")
        .and_then(|h| h.to_str().ok());

    // Parse the Expires header (RFC 9111 §5.3) into a Unix timestamp so we
    // can skip future fetches until the declared expiry time has passed.
    // Accepts all three HTTP-date formats (RFC 9110 §5.6.7).
    let mut expires: Option<i64> = resp
        .headers()
        .get("expires")
        .and_then(|h| h.to_str().ok())
        .and_then(parse_http_date)
        .map(|dt| dt.timestamp());

    let now_ts = Utc::now().timestamp();

    // Derive freshness hints before Cache-Control directives mutate
    // `expires` — the hint reflects the server's original instruction.
    let hints = extract_server_hints(resp.headers(), now_ts);

    // Parse Cache-Control and apply precedence rules (RFC 9111 §5.2):
    // - no-store: clear all cache headers
    // - no-cache: allow conditional requests but never skip fetching
    // - max-age: overrides Expires header (adjusted for upstream age per
    //   RFC 9111 §4.2.3)
    let cc_values: Vec<&str> = resp
        .headers()
        .get_all("cache-control")
        .iter()
        .filter_map(|h| h.to_str().ok())
        .collect();
    let cc = CacheControl::parse_many(&cc_values);
    if cc.no_store {
        etag = None;
        last_modified = None;
        expires = None;
    } else if cc.no_cache {
        expires = None;
    } else if let Some(max_age) = cc.max_age {
        let corrected = corrected_max_age(resp.headers(), max_age, now_ts);
        expires = Some(now_ts + corrected as i64);
    }

    // RFC 8246: while the response is fresh, skip conditional revalidation
    // entirely. Only meaningful when paired with a positive max-age.
    let immutable_until: Option<i64> = if hints.immutable {
        hints.hint_secs.and_then(|s| {
            if s > 0 {
                Some(now_ts.saturating_add(s as i64))
            } else {
                None
            }
        })
    } else {
        None
    };

    let next_fetch_at = compute_next_fetch_at(
        FetchOutcome::Success {
            server_hint_secs: hints.hint_secs,
        },
        now_ts,
        cfg.min_cadence,
        cfg.max_backoff,
        cfg.min_fetch_interval,
    );

    conn.execute(
        "UPDATE feeds SET
            header_etag = ?,
            header_last_modified = ?,
            header_expires = ?,
            header_immutable_until = ?,
            last_checked = ?,
            next_fetch_at = ?,
            consecutive_failures = 0,
            retry_after_at = NULL
         WHERE id = ?",
        (
            etag,
            last_modified,
            expires,
            immutable_until,
            now_ts,
            next_fetch_at,
            feed_id,
        ),
    )?;

    metrics.record_feed_retry_scheduled("cache_hint", (next_fetch_at - now_ts) as f64);

    let content = resp.bytes().await?;
    metrics.record_feed_response_bytes(content.len() as u64);
    Ok(Some(content.to_vec()))
}

fn retrieve_file_feed(
    feed_url: &str,
    feed_id: i64,
    pool: Pool<SqliteConnectionManager>,
    cfg: SchedulerConfig,
) -> Result<Option<Vec<u8>>> {
    let conn = pool.get()?;

    // Extract the file path from the URL
    let file_path = feed_url.strip_prefix("file://").unwrap_or(feed_url);

    // Read the file content
    let content = std::fs::read(file_path)
        .map_err(|e| anyhow::anyhow!("Failed to read file {}: {}", file_path, e))?;

    // File-backed feeds have no cache headers; schedule using the per-feed
    // interval (clamped to the global floor and ceiling).
    let now_ts = Utc::now().timestamp();
    let next_fetch_at = compute_next_fetch_at(
        FetchOutcome::Success {
            server_hint_secs: None,
        },
        now_ts,
        cfg.min_cadence,
        cfg.max_backoff,
        cfg.min_fetch_interval,
    );
    conn.execute(
        "UPDATE feeds SET
            last_checked = ?1,
            next_fetch_at = ?2,
            consecutive_failures = 0,
            retry_after_at = NULL
         WHERE id = ?3",
        (now_ts, next_fetch_at, feed_id),
    )?;

    Ok(Some(content))
}

/// Atom-specific feed-level data captured from a parsed feed.
#[derive(Default)]
struct AtomFeedIngestData {
    atom_uri: Option<String>,
    atom_language_tag: Option<String>,
    rights: Option<String>,
    generator: Option<atom_syndication::Generator>,
    logo: Option<String>,
    icon: Option<String>,
    authors: Vec<String>,
    contributors: Vec<String>,
    categories: Vec<atom_syndication::Category>,
}

/// Atom-specific per-entry data captured from a parsed entry.
#[derive(Default)]
struct AtomEntryIngestData {
    rights: Option<String>,
    authors: Vec<String>,
    contributors: Vec<String>,
    categories: Vec<atom_syndication::Category>,
}

/// RSS-specific per-entry data captured from a parsed item.
#[derive(Default)]
struct RssEntryIngestData {
    description: Option<String>,
    comments: Option<String>,
    author: Option<String>,
    enclosure_url: Option<String>,
    enclosure_length: Option<i64>,
    enclosure_mime_type: Option<String>,
    categories: Vec<rss::Category>,
}

fn extract_atom_feed_data(feed: &atom_syndication::Feed) -> AtomFeedIngestData {
    AtomFeedIngestData {
        atom_uri: feed.base.clone(),
        atom_language_tag: feed.lang.clone(),
        rights: feed.rights.as_ref().map(|r| r.value.clone()),
        generator: feed.generator.clone(),
        logo: feed.logo.clone(),
        icon: feed.icon.clone(),
        authors: feed.authors.iter().map(|p| p.name.clone()).collect(),
        contributors: feed.contributors.iter().map(|p| p.name.clone()).collect(),
        categories: feed.categories.clone(),
    }
}

/// Extract both a [`FeedEntry`] and the Atom-specific sub-object from an
/// Atom entry.
fn atom_entry_to_parts(
    feed_id: i64,
    entry: atom_syndication::Entry,
) -> (FeedEntry, AtomEntryIngestData) {
    let ingest = AtomEntryIngestData {
        rights: entry.rights.as_ref().map(|r| r.value.clone()),
        authors: entry.authors.iter().map(|p| p.name.clone()).collect(),
        contributors: entry.contributors.iter().map(|p| p.name.clone()).collect(),
        categories: entry.categories.clone(),
    };
    let feed_entry = FeedEntry {
        feed_id,
        syndication_format: "atom".to_string(),
        guid: entry.id,
        published_at: entry.published.map(|d| d.to_utc().timestamp()),
        title: entry.title.value,
        url: entry.links.into_iter().next().map(|l| l.href),
        content: entry.content.and_then(|c| c.value),
        tags: vec![],
    };
    (feed_entry, ingest)
}

/// Extract both a [`FeedEntry`] and the RSS-specific sub-object from an
/// RSS item.
fn rss_item_to_parts(feed_id: i64, item: rss::Item) -> (FeedEntry, RssEntryIngestData) {
    let rss::Item {
        pub_date,
        guid,
        title,
        link,
        description,
        author,
        comments,
        enclosure,
        categories,
        ..
    } = item;

    let timestamp = pub_date
        .as_deref()
        .and_then(|d| chrono::DateTime::parse_from_rfc2822(d).ok())
        .map(|d| d.timestamp())
        .unwrap_or_else(|| Utc::now().timestamp());

    let guid = guid.map(|g| g.value).unwrap_or_else(|| {
        format!(
            "rss-{}-{}",
            timestamp,
            title.as_deref().unwrap_or("no-title")
        )
    });

    let (enclosure_url, enclosure_length, enclosure_mime_type) = match enclosure {
        Some(e) => (Some(e.url), e.length.parse::<i64>().ok(), Some(e.mime_type)),
        None => (None, None, None),
    };

    let ingest = RssEntryIngestData {
        description: description.clone(),
        comments,
        author,
        enclosure_url,
        enclosure_length,
        enclosure_mime_type,
        categories,
    };

    let feed_entry = FeedEntry {
        feed_id,
        syndication_format: "rss".to_string(),
        guid,
        published_at: Some(timestamp),
        title: title.unwrap_or_default(),
        url: link,
        content: description,
        tags: vec![],
    };
    (feed_entry, ingest)
}

/// Upsert the single `atom_feed_data` row for `feed_id` and replace all
/// atom_feed_* child rows (authors, contributors, rights, generator, logo,
/// icon, categories). Children key directly on `feeds.id`, so the extra
/// `atom_feed_data.id` is not needed anywhere outside the row itself.
fn upsert_atom_feed_data(
    tx: &rusqlite::Transaction,
    feed_id: i64,
    data: &AtomFeedIngestData,
) -> Result<()> {
    tx.execute(
        "INSERT INTO atom_feed_data (feed_id, atom_uri, atom_language_tag)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(feed_id) DO UPDATE SET
             atom_uri = excluded.atom_uri,
             atom_language_tag = excluded.atom_language_tag",
        rusqlite::params![feed_id, data.atom_uri, data.atom_language_tag],
    )?;

    // Replace per-feed child rows. Rights/generator/logo/icon have a
    // feed_id PRIMARY KEY — one-per-feed semantics — so we delete-and-
    // reinsert to keep the logic uniform.
    tx.execute("DELETE FROM atom_feed_rights WHERE feed_id = ?1", [feed_id])?;
    if let Some(ref rights) = data.rights {
        tx.execute(
            "INSERT INTO atom_feed_rights (feed_id, rights) VALUES (?1, ?2)",
            rusqlite::params![feed_id, rights],
        )?;
    }

    tx.execute(
        "DELETE FROM atom_feed_generators WHERE feed_id = ?1",
        [feed_id],
    )?;
    if let Some(ref gen_) = data.generator {
        tx.execute(
            "INSERT INTO atom_feed_generators (feed_id, value, uri, version)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![feed_id, gen_.value, gen_.uri, gen_.version],
        )?;
    }

    tx.execute("DELETE FROM atom_feed_logos WHERE feed_id = ?1", [feed_id])?;
    if let Some(ref logo) = data.logo {
        tx.execute(
            "INSERT INTO atom_feed_logos (feed_id, uri) VALUES (?1, ?2)",
            rusqlite::params![feed_id, logo],
        )?;
    }

    tx.execute("DELETE FROM atom_feed_icons WHERE feed_id = ?1", [feed_id])?;
    if let Some(ref icon) = data.icon {
        tx.execute(
            "INSERT INTO atom_feed_icons (feed_id, uri) VALUES (?1, ?2)",
            rusqlite::params![feed_id, icon],
        )?;
    }

    tx.execute(
        "DELETE FROM atom_feed_authors WHERE feed_id = ?1",
        [feed_id],
    )?;
    for author in &data.authors {
        tx.execute(
            "INSERT INTO atom_feed_authors (feed_id, author) VALUES (?1, ?2)",
            rusqlite::params![feed_id, author],
        )?;
    }

    tx.execute(
        "DELETE FROM atom_feed_contributors WHERE feed_id = ?1",
        [feed_id],
    )?;
    for contributor in &data.contributors {
        tx.execute(
            "INSERT INTO atom_feed_contributors (feed_id, contributor) VALUES (?1, ?2)",
            rusqlite::params![feed_id, contributor],
        )?;
    }

    tx.execute(
        "DELETE FROM atom_feed_categories WHERE feed_id = ?1",
        [feed_id],
    )?;
    for cat in &data.categories {
        tx.execute(
            "INSERT INTO atom_categories (category, scheme, label)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![cat.term, cat.scheme, cat.label],
        )?;
        let cat_id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO atom_feed_categories (feed_id, category_id)
             VALUES (?1, ?2)",
            rusqlite::params![feed_id, cat_id],
        )?;
    }

    Ok(())
}

/// Insert the atom-specific child rows for a single entry. Children key
/// directly on `entries.id` and are cleared by the `INSERT OR REPLACE INTO
/// entries` CASCADE; we also delete defensively in case we're called on an
/// entry that wasn't replaced (e.g. a script-filtered re-ingest).
fn insert_atom_entry_data(
    tx: &rusqlite::Transaction,
    entry_id: i64,
    data: &AtomEntryIngestData,
) -> Result<()> {
    tx.execute(
        "DELETE FROM atom_entry_rights WHERE entry_id = ?1",
        [entry_id],
    )?;
    if let Some(ref rights) = data.rights {
        tx.execute(
            "INSERT INTO atom_entry_rights (entry_id, rights) VALUES (?1, ?2)",
            rusqlite::params![entry_id, rights],
        )?;
    }

    tx.execute(
        "DELETE FROM atom_entry_authors WHERE entry_id = ?1",
        [entry_id],
    )?;
    for author in &data.authors {
        tx.execute(
            "INSERT INTO atom_entry_authors (entry_id, author) VALUES (?1, ?2)",
            rusqlite::params![entry_id, author],
        )?;
    }

    tx.execute(
        "DELETE FROM atom_entry_contributors WHERE entry_id = ?1",
        [entry_id],
    )?;
    for contributor in &data.contributors {
        tx.execute(
            "INSERT INTO atom_entry_contributors (entry_id, contributor) VALUES (?1, ?2)",
            rusqlite::params![entry_id, contributor],
        )?;
    }

    tx.execute(
        "DELETE FROM atom_entry_categories WHERE entry_id = ?1",
        [entry_id],
    )?;
    for cat in &data.categories {
        tx.execute(
            "INSERT INTO atom_categories (category, scheme, label)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![cat.term, cat.scheme, cat.label],
        )?;
        let cat_id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO atom_entry_categories (entry_id, category_id)
             VALUES (?1, ?2)",
            rusqlite::params![entry_id, cat_id],
        )?;
    }

    Ok(())
}

/// Insert the RSS-specific child rows for a single entry.
fn insert_rss_entry_data(
    tx: &rusqlite::Transaction,
    entry_id: i64,
    data: &RssEntryIngestData,
) -> Result<()> {
    tx.execute("DELETE FROM rss_entry_data WHERE entry_id = ?1", [entry_id])?;
    tx.execute(
        "INSERT INTO rss_entry_data (
            entry_id, description, comments, author,
            enclosure_url, enclosure_length, enclosure_mime_type
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            entry_id,
            data.description,
            data.comments,
            data.author,
            data.enclosure_url,
            data.enclosure_length,
            data.enclosure_mime_type,
        ],
    )?;

    tx.execute("DELETE FROM rss_categories WHERE entry_id = ?1", [entry_id])?;
    for cat in &data.categories {
        tx.execute(
            "INSERT OR IGNORE INTO rss_categories (entry_id, category, domain)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![entry_id, cat.name, cat.domain],
        )?;
    }

    Ok(())
}

/// Download and cache the external assets referenced by an entry.
///
/// Runs in an async worker: looks up the entry's post-script `content` HTML
/// plus its feed URL and any RSS enclosure, extracts `<img src>` URLs, and
/// delegates each to [`assets::cache_asset`]. Individual asset failures are
/// logged and skipped.
pub(crate) async fn cache_entry_assets(
    client: &reqwest::Client,
    pool: &Pool<SqliteConnectionManager>,
    data_dir: &std::path::Path,
    entry_id: i64,
) -> Result<()> {
    #[derive(Debug)]
    struct EntryCtx {
        content: Option<String>,
        base: Option<String>,
        enclosure_url: Option<String>,
    }

    let ctx = {
        let conn = pool.get()?;
        if !crate::db::assets::get_cache_enabled(&conn)? {
            return Ok(());
        }
        let row = conn
            .query_row(
                "SELECT e.content, f.url, e.url, red.enclosure_url
                 FROM entries e
                 LEFT JOIN feeds f ON f.id = e.feed_id
                 LEFT JOIN rss_entry_data red ON red.entry_id = e.id
                 WHERE e.id = ?1",
                [entry_id],
                |row| {
                    let content: Option<String> = row.get(0)?;
                    let feed_url: Option<String> = row.get(1)?;
                    let entry_url: Option<String> = row.get(2)?;
                    let enclosure_url: Option<String> = row.get(3)?;
                    // Prefer the entry URL as the resolution base; fall back
                    // to the feed URL so relative URLs still work when the
                    // entry URL is empty.
                    let base = entry_url.filter(|s| !s.is_empty()).or(feed_url);
                    Ok(EntryCtx {
                        content,
                        base,
                        enclosure_url,
                    })
                },
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        match row {
            Some(r) => r,
            None => return Ok(()),
        }
    };

    let base = match ctx.base.as_deref().and_then(|s| Url::parse(s).ok()) {
        Some(u) => u,
        None => {
            debug!(
                "entry {} has no resolvable base URL; skipping asset cache",
                entry_id
            );
            return Ok(());
        }
    };

    // Inline images from the entry's HTML content.
    if let Some(content) = ctx.content.as_deref() {
        for url in assets::extract_asset_urls(content, &base) {
            if let Err(e) = assets::cache_asset(
                client,
                pool,
                data_dir,
                &url,
                entry_id,
                assets::AssetKind::InlineImg,
            )
            .await
            {
                warn!("cache_asset failed for {}: {:?}", url, e);
            }
        }
    }

    // RSS enclosure, when present.
    if let Some(enc) = ctx.enclosure_url.as_deref() {
        if let Some(url) = Url::parse(enc)
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https"))
        {
            if let Err(e) = assets::cache_asset(
                client,
                pool,
                data_dir,
                &url,
                entry_id,
                assets::AssetKind::Enclosure,
            )
            .await
            {
                warn!("cache_asset failed for enclosure {}: {:?}", url, e);
            }
        }
    }

    Ok(())
}

fn process_atom_feed(
    feed_id: i64,
    feed: atom_syndication::Feed,
    mut conn: PooledConnection<SqliteConnectionManager>,
    script_runner: Option<&dyn ScriptRunner>,
    metrics: &Metrics,
) -> Result<Vec<i64>> {
    info!(
        "Successfully fetched Atom feed {} with {} items",
        feed_id,
        feed.entries.len()
    );

    let feed_data = extract_atom_feed_data(&feed);

    {
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE feeds SET syndication_format = 'atom' WHERE id = ?1",
            [feed_id],
        )?;
        upsert_atom_feed_data(&tx, feed_id, &feed_data)?;
        tx.commit()?;
    }

    let mut inserted_entry_ids: Vec<i64> = Vec::new();
    for entry in feed.entries.into_iter() {
        let (feed_entry, ingest) = atom_entry_to_parts(feed_id, entry);

        let feed_entry = if let Some(runner) = script_runner {
            let original = feed_entry.clone();
            let script_start = Instant::now();
            match runner.process_entry(feed_entry) {
                Ok(Some(e)) => {
                    metrics.record_script_execution(script_start.elapsed().as_secs_f64(), "ok");
                    e
                }
                Ok(None) => {
                    metrics
                        .record_script_execution(script_start.elapsed().as_secs_f64(), "filtered");
                    debug!("atom entry filtered by script for feed {}", feed_id);
                    continue;
                }
                Err(e) => {
                    metrics.record_script_execution(script_start.elapsed().as_secs_f64(), "error");
                    warn!(
                        "script error processing atom entry for feed {}: {}; inserting unmodified",
                        feed_id, e
                    );
                    original
                }
            }
        } else {
            feed_entry
        };

        let tx = conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO entries (
                feed_id,
                syndication_format,
                guid,
                published_at,
                title,
                url,
                content
            ) VALUES (?1, 'atom', ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                feed_id,
                feed_entry.guid,
                feed_entry.published_at,
                feed_entry.title,
                feed_entry.url,
                feed_entry.content
            ],
        )?;

        let entry_id: i64 = tx.query_row(
            "SELECT id FROM entries WHERE feed_id = ?1 AND guid = ?2",
            rusqlite::params![feed_id, feed_entry.guid],
            |row| row.get(0),
        )?;

        insert_atom_entry_data(&tx, entry_id, &ingest)?;
        tx.commit()?;
        metrics.record_feed_entry_upserted("atom");
        inserted_entry_ids.push(entry_id);

        if !feed_entry.tags.is_empty() {
            sync_entry_tags(&conn, feed_id, &feed_entry.guid, &feed_entry.tags)?;
        }
    }

    Ok(inserted_entry_ids)
}

fn process_rss_feed(
    feed_id: i64,
    channel: rss::Channel,
    mut conn: PooledConnection<SqliteConnectionManager>,
    script_runner: Option<&dyn ScriptRunner>,
    metrics: &Metrics,
) -> Result<Vec<i64>> {
    info!(
        "Successfully fetched RSS feed {} with {} items",
        feed_id,
        channel.items.len()
    );

    conn.execute(
        "UPDATE feeds SET syndication_format = 'rss' WHERE id = ?1",
        [feed_id],
    )?;

    let mut inserted_entry_ids: Vec<i64> = Vec::new();
    for item in channel.items.into_iter() {
        let (feed_entry, ingest) = rss_item_to_parts(feed_id, item);

        let feed_entry = if let Some(runner) = script_runner {
            let original = feed_entry.clone();
            let script_start = Instant::now();
            match runner.process_entry(feed_entry) {
                Ok(Some(e)) => {
                    metrics.record_script_execution(script_start.elapsed().as_secs_f64(), "ok");
                    e
                }
                Ok(None) => {
                    metrics
                        .record_script_execution(script_start.elapsed().as_secs_f64(), "filtered");
                    debug!("rss entry filtered by script for feed {}", feed_id);
                    continue;
                }
                Err(e) => {
                    metrics.record_script_execution(script_start.elapsed().as_secs_f64(), "error");
                    warn!(
                        "script error processing rss entry for feed {}: {}; inserting unmodified",
                        feed_id, e
                    );
                    original
                }
            }
        } else {
            feed_entry
        };

        let tx = conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO entries (
                feed_id,
                syndication_format,
                guid,
                published_at,
                title,
                url,
                content
            ) VALUES (?1, 'rss', ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                feed_id,
                feed_entry.guid,
                feed_entry.published_at,
                feed_entry.title,
                feed_entry.url,
                feed_entry.content
            ],
        )?;

        let entry_id: i64 = tx.query_row(
            "SELECT id FROM entries WHERE feed_id = ?1 AND guid = ?2",
            rusqlite::params![feed_id, feed_entry.guid],
            |row| row.get(0),
        )?;

        insert_rss_entry_data(&tx, entry_id, &ingest)?;
        tx.commit()?;
        metrics.record_feed_entry_upserted("rss");
        inserted_entry_ids.push(entry_id);

        if !feed_entry.tags.is_empty() {
            sync_entry_tags(&conn, feed_id, &feed_entry.guid, &feed_entry.tags)?;
        }
    }

    Ok(inserted_entry_ids)
}

/// Enqueue a [`TaskManagerCommand::CacheEntryAssets`] for each of the given
/// entry IDs. Best-effort: a full or closed queue is logged and ignored.
fn enqueue_asset_caching(
    task_tx: &async_channel::Sender<TaskManagerCommand>,
    metrics: &Metrics,
    entry_ids: &[i64],
) {
    for &entry_id in entry_ids {
        match task_tx.try_send(TaskManagerCommand::CacheEntryAssets { entry_id }) {
            Ok(()) => metrics.record_task_enqueued("cache_entry_assets"),
            Err(e) => debug!(
                "failed to queue CacheEntryAssets for entry {}: {:?}",
                entry_id, e
            ),
        }
    }
}

/// Resolve and sync the script-provided tags for a newly inserted database entry.
///
/// For each tag name in `tags`:
/// - ensures the tag row exists in `tags` (`INSERT OR IGNORE`)
/// - looks up its `id`
///
/// Then removes any `entry_tags` rows for this entry whose `tag_id` is not in the
/// script-provided set, and inserts new associations (`INSERT OR IGNORE`).
fn sync_entry_tags(
    conn: &PooledConnection<SqliteConnectionManager>,
    feed_id: i64,
    guid: &str,
    tags: &[String],
) -> Result<()> {
    let entry_id: i64 = conn.query_row(
        "SELECT id FROM entries WHERE feed_id = ?1 AND guid = ?2",
        rusqlite::params![feed_id, guid],
        |row| row.get(0),
    )?;

    // Upsert each tag and collect its id.
    let mut tag_ids: Vec<i64> = Vec::with_capacity(tags.len());
    for name in tags {
        conn.execute("INSERT OR IGNORE INTO tags (name) VALUES (?1)", [name])?;
        let id: i64 = conn.query_row(
            "SELECT id FROM tags WHERE name = ?1",
            [name.as_str()],
            |row| row.get(0),
        )?;
        tag_ids.push(id);
    }

    // Remove stale entry_tags rows (those not in the script-provided set).
    if tag_ids.is_empty() {
        conn.execute("DELETE FROM entry_tags WHERE entry_id = ?1", [entry_id])?;
    } else {
        let placeholders = tag_ids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 2))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "DELETE FROM entry_tags WHERE entry_id = ?1 AND tag_id NOT IN ({placeholders})"
        );
        let params: Vec<rusqlite::types::Value> =
            std::iter::once(rusqlite::types::Value::Integer(entry_id))
                .chain(
                    tag_ids
                        .iter()
                        .map(|&id| rusqlite::types::Value::Integer(id)),
                )
                .collect();
        conn.execute(&sql, rusqlite::params_from_iter(params))?;
    }

    // Insert new tag associations.
    for tag_id in tag_ids {
        conn.execute(
            "INSERT OR IGNORE INTO entry_tags (entry_id, tag_id) VALUES (?1, ?2)",
            rusqlite::params![entry_id, tag_id],
        )?;
    }

    Ok(())
}
