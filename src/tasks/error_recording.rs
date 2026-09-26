use crate::metrics::Metrics;
use crate::tasks::backoff::{compute_next_fetch_at, FetchOutcome};
use crate::tasks::error::FetchError;
use chrono::Utc;
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use tracing::error;

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
pub(super) fn set_feed_error_with_schedule(
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
pub(super) fn set_feed_error(
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
pub(super) fn clear_feed_error(conn: &PooledConnection<SqliteConnectionManager>, feed_id: i64) {
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
