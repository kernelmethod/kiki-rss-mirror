use chrono::{DateTime, Datelike, Timelike, Utc};
use reqwest::Url;

/// Return true if two URL strings share the same (scheme, host, port) origin.
///
/// Used to decide whether credentials intended for a feed's configured URL
/// may be forwarded across a redirect. Parse failures are treated as "not
/// same-origin" — we refuse to leak credentials to a URL we can't inspect.
pub(crate) fn same_origin(a: &str, b: &str) -> bool {
    let (Ok(ua), Ok(ub)) = (Url::parse(a), Url::parse(b)) else {
        return false;
    };
    ua.scheme() == ub.scheme()
        && ua.host_str() == ub.host_str()
        && ua.port_or_known_default() == ub.port_or_known_default()
}

/// Parse an HTTP `Retry-After` header value into an absolute Unix timestamp.
///
/// RFC 7231 §7.1.3 allows either a non-negative integer delta-seconds or an
/// HTTP-date. Returns `None` if the value cannot be parsed.
pub(super) fn parse_retry_after(value: &str, now: DateTime<Utc>) -> Option<i64> {
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
pub(super) enum FetchOutcome {
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

/// Settings controlling the fetch scheduler, read together so `refresh_feed`
/// and its helpers don't hit the database separately.
#[derive(Debug, Clone, Copy)]
pub(super) struct SchedulerConfig {
    pub(super) min_cadence: u64,
    pub(super) max_backoff: u64,
    pub(super) min_fetch_interval: u64,
    /// How often (seconds) to bypass conditional-request headers and force
    /// a full `GET` so the fetcher can detect servers that keep returning
    /// unchanged validators while the body has actually changed.
    pub(super) force_refresh_after: u64,
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
pub(super) fn compute_next_fetch_at(
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

/// Move `ts` forward to the first hour the feed has not asked to be left
/// alone, per RSS `<skipHours>` / `<skipDays>` (both in UTC).
///
/// `skip_hours` and `skip_days` are the bitmasks from
/// [`crate::fetcher::FeedHints`]. A deferred time lands on the top of the
/// first allowed hour. Masks that rule out every hour or every day are
/// treated as a publisher mistake and ignored, rather than never fetching
/// the feed again.
pub(super) fn defer_past_skipped(ts: i64, skip_hours: u32, skip_days: u8) -> i64 {
    const ALL_HOURS: u32 = (1 << 24) - 1;
    const ALL_DAYS: u8 = (1 << 7) - 1;
    let skip_hours = skip_hours & ALL_HOURS;
    let skip_days = skip_days & ALL_DAYS;
    if (skip_hours == 0 && skip_days == 0) || skip_hours == ALL_HOURS || skip_days == ALL_DAYS {
        return ts;
    }

    let mut candidate = ts;
    // Six skipped days plus a day of skipped hours is the longest possible
    // run, so eight days of hours always reaches an allowed one.
    for _ in 0..(8 * 24) {
        let Some(dt) = DateTime::<Utc>::from_timestamp(candidate, 0) else {
            return ts;
        };
        let hour_skipped = skip_hours & (1 << dt.hour()) != 0;
        let day_skipped = skip_days & (1 << dt.weekday().num_days_from_monday()) != 0;
        if !hour_skipped && !day_skipped {
            return candidate;
        }
        candidate = candidate - candidate.rem_euclid(3600) + 3600;
    }
    ts
}
