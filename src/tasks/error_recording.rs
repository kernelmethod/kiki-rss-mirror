use crate::metrics::Metrics;
use crate::tasks::backoff::{plan_next_fetch, FetchOutcome, Schedule};
use crate::tasks::error::FetchError;
use chrono::Utc;
use rusqlite::Connection;
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
///
/// Returns when the feed will next be attempted and why, or `None` if the
/// error could not be recorded (which is logged here).
#[allow(clippy::too_many_arguments)]
pub(super) fn set_feed_error_with_schedule(
    conn: &Connection,
    feed_id: i64,
    fetch_error: &FetchError,
    retry_after_ts: Option<i64>,
    stale_if_error_secs: Option<u64>,
    min_cadence: u64,
    max_backoff: u64,
    min_fetch_interval: u64,
    metrics: &Metrics,
) -> Option<Schedule> {
    let json = match serde_json::to_string(fetch_error) {
        Ok(j) => j,
        Err(e) => {
            error!(
                "Failed to serialize fetch error for feed {}: {:?}",
                feed_id, e
            );
            return None;
        }
    };

    let now_ts = Utc::now().timestamp();
    let is_transient = fetch_error.is_transient();

    // Read the current failure count so we can compute the new streak length
    // without a race window, along with the feed's own skipHours/skipDays so
    // retries also stay out of them. Default to 0 if the row is somehow
    // missing.
    let (current_failures, skip_hours, skip_days): (u32, u32, u8) = conn
        .query_row(
            "SELECT consecutive_failures, feed_skip_hours, feed_skip_days
             FROM feeds WHERE id = ?1",
            [feed_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?.max(0) as u32,
                    row.get::<_, i64>(1)? as u32,
                    row.get::<_, i64>(2)? as u8,
                ))
            },
        )
        .unwrap_or((0, 0, 0));
    let new_failures = current_failures.saturating_add(1);

    let outcome = if is_transient {
        FetchOutcome::TransientErr {
            retry_after_ts,
            consecutive_failures: new_failures,
            stale_if_error_secs,
        }
    } else {
        FetchOutcome::PermanentErr
    };

    let schedule = plan_next_fetch(
        outcome,
        now_ts,
        min_cadence,
        max_backoff,
        min_fetch_interval,
    )
    .defer_past_skipped(skip_hours, skip_days);
    let next_fetch_at = schedule.next_fetch_at;

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
        return None;
    }

    metrics.record_feed_retry_scheduled(
        schedule.reason.metric_source(),
        (next_fetch_at - now_ts) as f64,
    );
    metrics.record_feed_consecutive_failures(new_failures);
    Some(schedule)
}

/// Back-compat wrapper around [`set_feed_error_with_schedule`] for callsites
/// that don't have a `Retry-After` timestamp. Takes the scheduler tuning
/// from `settings`, and returns the same as
/// [`set_feed_error_with_schedule`].
pub(super) fn set_feed_error(
    conn: &Connection,
    feed_id: i64,
    fetch_error: &FetchError,
    settings: &crate::config::FeedFetchSettings,
    metrics: &Metrics,
) -> Option<Schedule> {
    let min_cadence = settings.min_polling_cadence_seconds;
    let max_backoff = settings.max_backoff_seconds;
    let min_fetch_interval = conn
        .query_row(
            "SELECT min_fetch_interval_seconds FROM feeds WHERE id = ?1",
            [feed_id],
            |row| row.get::<_, i64>(0),
        )
        .map(|v| v.max(0) as u64)
        .unwrap_or(settings.default_fetch_interval_seconds);

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
    )
}

/// Clear any previously recorded fetch error for a feed and reset the failure
/// streak counter.
pub(super) fn clear_feed_error(conn: &Connection, feed_id: i64) {
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
