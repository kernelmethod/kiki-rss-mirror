#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::super::backoff::{
    compute_next_fetch_at, defer_past_skipped, format_duration, plan_next_fetch, same_origin,
    FetchOutcome, ScheduleClamp, ScheduleReason,
};
use super::super::FetchError;

const MIN_CADENCE: u64 = 60;
const MAX_BACKOFF: u64 = 86_400;
const MIN_FETCH_INTERVAL: u64 = 10_800;

fn success_with_hint(hint: Option<u64>) -> FetchOutcome {
    FetchOutcome::Success {
        server_hint_secs: hint,
    }
}

/// Transient vs. permanent classification for every [`FetchError`] variant.
#[test]
fn test_is_transient_classification() {
    let url = "https://example.com/feed".to_string();

    // Transient: 408, 429, any 5xx
    assert!(FetchError::HttpStatus {
        url: url.clone(),
        status: 408
    }
    .is_transient());
    assert!(FetchError::HttpStatus {
        url: url.clone(),
        status: 429
    }
    .is_transient());
    assert!(FetchError::HttpStatus {
        url: url.clone(),
        status: 500
    }
    .is_transient());
    assert!(FetchError::HttpStatus {
        url: url.clone(),
        status: 502
    }
    .is_transient());
    assert!(FetchError::HttpStatus {
        url: url.clone(),
        status: 599
    }
    .is_transient());
    assert!(FetchError::Other {
        message: "connect: reset".into()
    }
    .is_transient());

    // Permanent: other 4xx, InvalidFeed, TooManyRedirects
    assert!(!FetchError::HttpStatus {
        url: url.clone(),
        status: 400
    }
    .is_transient());
    assert!(!FetchError::HttpStatus {
        url: url.clone(),
        status: 401
    }
    .is_transient());
    assert!(!FetchError::HttpStatus {
        url: url.clone(),
        status: 403
    }
    .is_transient());
    assert!(!FetchError::HttpStatus {
        url: url.clone(),
        status: 404
    }
    .is_transient());
    assert!(!FetchError::InvalidFeed { url: url.clone() }.is_transient());
    assert!(!FetchError::TooManyRedirects { url }.is_transient());
}

/// When the server provides a `max-age` hint shorter than the per-feed
/// interval, the hint wins.
#[test]
fn test_compute_next_fetch_at_success_uses_server_hint() {
    let now = 1_000_000;
    let next = compute_next_fetch_at(
        success_with_hint(Some(300)),
        now,
        MIN_CADENCE,
        MAX_BACKOFF,
        MIN_FETCH_INTERVAL,
    );
    assert_eq!(next, now + 300);
}

/// The per-feed `min_fetch_interval_seconds` is a hard ceiling: even a
/// generous `max-age=86400` is clamped to a feed's 3600s interval.
#[test]
fn test_compute_next_fetch_at_caps_at_min_fetch_interval() {
    let now = 1_000_000;
    let next = compute_next_fetch_at(
        success_with_hint(Some(86_400)),
        now,
        MIN_CADENCE,
        MAX_BACKOFF,
        3_600, // per-feed cap
    );
    assert_eq!(next, now + 3_600);
}

/// A server advertising a very short `max-age` cannot push the interval
/// below the global `min_polling_cadence_seconds` floor.
#[test]
fn test_compute_next_fetch_at_floors_at_min_cadence() {
    let now = 1_000_000;
    let next = compute_next_fetch_at(
        success_with_hint(Some(5)),
        now,
        MIN_CADENCE,
        MAX_BACKOFF,
        MIN_FETCH_INTERVAL,
    );
    assert_eq!(next, now + MIN_CADENCE as i64);
}

/// With no server hint we fall back to the per-feed interval.
#[test]
fn test_compute_next_fetch_at_no_hint_uses_min_fetch_interval() {
    let now = 1_000_000;
    let next = compute_next_fetch_at(
        success_with_hint(None),
        now,
        MIN_CADENCE,
        MAX_BACKOFF,
        MIN_FETCH_INTERVAL,
    );
    assert_eq!(next, now + MIN_FETCH_INTERVAL as i64);
}

/// Consecutive failures double the backoff interval, starting at the
/// `min_polling_cadence` floor.
#[test]
fn test_compute_next_fetch_at_exponential_backoff() {
    let now = 1_000_000;
    for (failures, expected) in [(1u32, 60), (2, 120), (3, 240), (4, 480), (5, 960)] {
        let next = compute_next_fetch_at(
            FetchOutcome::TransientErr {
                retry_after_ts: None,
                consecutive_failures: failures,
                stale_if_error_secs: None,
            },
            now,
            MIN_CADENCE,
            MAX_BACKOFF,
            MIN_FETCH_INTERVAL,
        );
        assert_eq!(
            next,
            now + expected,
            "failure #{failures} should produce {expected}s backoff"
        );
    }
}

/// Very long failure streaks do not overflow — the backoff is clamped to
/// the configured cap.
#[test]
fn test_compute_next_fetch_at_backoff_caps_at_max_backoff() {
    let now = 1_000_000;
    let next = compute_next_fetch_at(
        FetchOutcome::TransientErr {
            retry_after_ts: None,
            consecutive_failures: 20,
            stale_if_error_secs: None,
        },
        now,
        MIN_CADENCE,
        MAX_BACKOFF,
        MIN_FETCH_INTERVAL,
    );
    assert_eq!(next, now + MAX_BACKOFF as i64);
}

/// Permanent errors wait the full backoff cap — no fast retry.
#[test]
fn test_compute_next_fetch_at_permanent_uses_max_backoff() {
    let now = 1_000_000;
    let next = compute_next_fetch_at(
        FetchOutcome::PermanentErr,
        now,
        MIN_CADENCE,
        MAX_BACKOFF,
        MIN_FETCH_INTERVAL,
    );
    assert_eq!(next, now + MAX_BACKOFF as i64);
}

/// Unix timestamp for a UTC date and time, for the skip-window tests.
fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
    chrono::NaiveDate::from_ymd_opt(y, mo, d)
        .unwrap()
        .and_hms_opt(h, mi, 0)
        .unwrap()
        .and_utc()
        .timestamp()
}

const MONDAY: u8 = 1 << 0;
const SATURDAY: u8 = 1 << 5;
const SUNDAY: u8 = 1 << 6;

/// Without `<skipHours>` / `<skipDays>` the schedule is untouched.
#[test]
fn test_defer_past_skipped_no_masks() {
    let ts = utc(2024, 1, 3, 10, 17);
    assert_eq!(defer_past_skipped(ts, 0, 0), ts);
}

/// A time outside every skip window is untouched.
#[test]
fn test_defer_past_skipped_allowed_time_unchanged() {
    // Wednesday 10:17, skipping 02:00-03:59 and weekends.
    let ts = utc(2024, 1, 3, 10, 17);
    assert_eq!(
        defer_past_skipped(ts, (1 << 2) | (1 << 3), SATURDAY | SUNDAY),
        ts
    );
}

/// A time in a skipped hour moves to the top of the next allowed hour.
#[test]
fn test_defer_past_skipped_hours() {
    let ts = utc(2024, 1, 3, 2, 40);
    assert_eq!(
        defer_past_skipped(ts, (1 << 2) | (1 << 3), 0),
        utc(2024, 1, 3, 4, 0)
    );
}

/// Skipped hours wrap past midnight into the next day.
#[test]
fn test_defer_past_skipped_hours_wrap_midnight() {
    let ts = utc(2024, 1, 3, 23, 5);
    assert_eq!(
        defer_past_skipped(ts, (1 << 23) | (1 << 0), 0),
        utc(2024, 1, 4, 1, 0)
    );
}

/// A skipped day moves the fetch to midnight of the next allowed day.
#[test]
fn test_defer_past_skipped_days() {
    // Saturday 2024-01-06 15:00, skipping the weekend.
    let ts = utc(2024, 1, 6, 15, 0);
    assert_eq!(
        defer_past_skipped(ts, 0, SATURDAY | SUNDAY),
        utc(2024, 1, 8, 0, 0)
    );
}

/// Skipped days and hours combine: out of the weekend, then past Monday's
/// skipped early hours.
#[test]
fn test_defer_past_skipped_days_and_hours() {
    let ts = utc(2024, 1, 6, 23, 30);
    assert_eq!(
        defer_past_skipped(ts, (1 << 0) | (1 << 1), SATURDAY | SUNDAY),
        utc(2024, 1, 8, 2, 0)
    );
}

/// A feed that skips every hour or every day would never be fetched again;
/// that is treated as a mistake and the skip masks are ignored.
#[test]
fn test_defer_past_skipped_all_skipped_is_ignored() {
    let ts = utc(2024, 1, 3, 10, 17);
    assert_eq!(defer_past_skipped(ts, (1 << 24) - 1, 0), ts);
    assert_eq!(defer_past_skipped(ts, 0, 0x7f), ts);
    // Bits outside the valid range don't count toward "everything".
    assert_eq!(
        defer_past_skipped(ts, !(1 << 11), MONDAY),
        utc(2024, 1, 3, 11, 0)
    );
}

/// `stale-if-error` shorter than `Retry-After` caps the retry, so we try
/// again before the grace window closes (RFC 5861 §4).
#[test]
fn test_compute_next_fetch_at_stale_if_error_caps_retry_after() {
    let now = 1_000_000;
    let next = compute_next_fetch_at(
        FetchOutcome::TransientErr {
            retry_after_ts: Some(now + 7200),
            consecutive_failures: 1,
            stale_if_error_secs: Some(1800),
        },
        now,
        MIN_CADENCE,
        MAX_BACKOFF,
        MIN_FETCH_INTERVAL,
    );
    assert_eq!(next, now + 1800);
}

/// A `Retry-After` shorter than `stale-if-error` is honored as is.
#[test]
fn test_compute_next_fetch_at_retry_after_within_stale_if_error() {
    let now = 1_000_000;
    let next = compute_next_fetch_at(
        FetchOutcome::TransientErr {
            retry_after_ts: Some(now + 600),
            consecutive_failures: 1,
            stale_if_error_secs: Some(3600),
        },
        now,
        MIN_CADENCE,
        MAX_BACKOFF,
        MIN_FETCH_INTERVAL,
    );
    assert_eq!(next, now + 600);
}

/// A tiny `stale-if-error` still can't push a retry below the global
/// `min_polling_cadence` floor.
#[test]
fn test_compute_next_fetch_at_stale_if_error_respects_min_cadence() {
    let now = 1_000_000;
    let next = compute_next_fetch_at(
        FetchOutcome::TransientErr {
            retry_after_ts: Some(now + 7200),
            consecutive_failures: 1,
            stale_if_error_secs: Some(5),
        },
        now,
        MIN_CADENCE,
        MAX_BACKOFF,
        MIN_FETCH_INTERVAL,
    );
    assert_eq!(next, now + MIN_CADENCE as i64);
}

/// Credentials may only follow a redirect when scheme, host and port all
/// match the feed's URL; any one differing makes the origin foreign.
#[test]
fn test_same_origin() {
    let feed = "https://example.com/feed";
    assert!(same_origin(feed, "https://example.com/other?x=1"));
    // An explicit default port is the same origin as an implied one.
    assert!(same_origin(feed, "https://example.com:443/feed"));

    assert!(!same_origin(feed, "http://example.com/feed"));
    assert!(!same_origin(feed, "https://evil.example.net/feed"));
    assert!(!same_origin(feed, "https://example.com:8443/feed"));
    // Unparseable URLs are never trusted.
    assert!(!same_origin(feed, "not a url"));
}

/// 2026-09-28T18:39:42Z, a Monday.
const PLAN_NOW: i64 = 1_790_620_782;

fn plan(outcome: FetchOutcome) -> super::super::backoff::Schedule {
    plan_next_fetch(
        outcome,
        PLAN_NOW,
        MIN_CADENCE,
        MAX_BACKOFF,
        MIN_FETCH_INTERVAL,
    )
}

/// A zero freshness hint is raised to the minimum cadence, and the
/// explanation says so, naming where the hint came from.
#[test]
fn test_plan_zero_hint_explains_min_cadence() {
    let schedule = plan(FetchOutcome::NotModified {
        server_hint_secs: Some(0),
    })
    .with_hint_source(Some("Cache-Control \"public, max-age=0\"".to_string()));
    assert_eq!(schedule.next_fetch_at, PLAN_NOW + 60);
    assert_eq!(
        schedule.reason,
        ScheduleReason::FreshnessHint {
            secs: 0,
            interval_secs: MIN_FETCH_INTERVAL
        }
    );
    assert_eq!(schedule.clamp, Some(ScheduleClamp::MinCadence { secs: 60 }));
    assert_eq!(
        schedule.to_string(),
        "in 1m at 2026-09-28T18:40:42Z (freshness hint of 0s, under the feed's 3h \
         interval; hint from Cache-Control \"public, max-age=0\", raised to the 1m \
         minimum polling cadence)"
    );
}

/// Without a hint, or with one longer than the interval, the feed's own
/// interval decides, and a longer hint is mentioned.
#[test]
fn test_plan_feed_interval_reasons() {
    let schedule = plan(success_with_hint(None));
    assert_eq!(schedule.next_fetch_at, PLAN_NOW + MIN_FETCH_INTERVAL as i64);
    assert_eq!(
        schedule.reason,
        ScheduleReason::FeedInterval {
            secs: MIN_FETCH_INTERVAL,
            hint_secs: None
        }
    );
    assert_eq!(schedule.clamp, None);
    assert_eq!(
        schedule.to_string(),
        "in 3h at 2026-09-28T21:39:42Z (feed interval of 3h, no freshness hint)"
    );

    let schedule = plan(success_with_hint(Some(86_400)))
        .with_hint_source(Some("the feed's <ttl>/sy:updatePeriod".to_string()));
    assert_eq!(
        schedule.reason,
        ScheduleReason::FeedInterval {
            secs: MIN_FETCH_INTERVAL,
            hint_secs: Some(86_400)
        }
    );
    assert_eq!(
        schedule.to_string(),
        "in 3h at 2026-09-28T21:39:42Z (feed interval of 3h, freshness hint of 1d; \
         hint from the feed's <ttl>/sy:updatePeriod)"
    );
}

/// The hint source is left out when no hint was used.
#[test]
fn test_plan_hint_source_hidden_without_hint() {
    let schedule = plan(success_with_hint(None)).with_hint_source(Some("x".to_string()));
    assert!(!schedule.to_string().contains("hint from"));
}

/// Error outcomes name the rule that picked the retry time.
#[test]
fn test_plan_error_reasons() {
    let schedule = plan(FetchOutcome::TransientErr {
        retry_after_ts: None,
        consecutive_failures: 3,
        stale_if_error_secs: None,
    });
    assert_eq!(schedule.next_fetch_at, PLAN_NOW + 240);
    assert_eq!(
        schedule.reason,
        ScheduleReason::Backoff {
            consecutive_failures: 3
        }
    );
    assert!(schedule
        .to_string()
        .ends_with("(backoff after 3 consecutive failures)"));

    let schedule = plan(FetchOutcome::TransientErr {
        retry_after_ts: Some(PLAN_NOW + 600),
        consecutive_failures: 1,
        stale_if_error_secs: None,
    });
    assert_eq!(schedule.next_fetch_at, PLAN_NOW + 600);
    assert_eq!(schedule.reason, ScheduleReason::RetryAfter);

    let schedule = plan(FetchOutcome::TransientErr {
        retry_after_ts: Some(PLAN_NOW + 6_000),
        consecutive_failures: 1,
        stale_if_error_secs: Some(300),
    });
    assert_eq!(schedule.next_fetch_at, PLAN_NOW + 300);
    assert_eq!(schedule.reason, ScheduleReason::StaleIfError { secs: 300 });

    let schedule = plan(FetchOutcome::TransientErr {
        retry_after_ts: None,
        consecutive_failures: 30,
        stale_if_error_secs: None,
    });
    assert_eq!(schedule.next_fetch_at, PLAN_NOW + MAX_BACKOFF as i64);
    assert_eq!(
        schedule.clamp,
        Some(ScheduleClamp::MaxBackoff { secs: MAX_BACKOFF })
    );
    assert!(schedule
        .to_string()
        .ends_with("(backoff after 30 consecutive failures, capped at the 1d maximum backoff)"));

    // A permanent error waits the cap by design; it isn't reported as capped.
    let schedule = plan(FetchOutcome::PermanentErr);
    assert_eq!(schedule.reason, ScheduleReason::PermanentError);
    assert_eq!(schedule.clamp, None);
    assert!(schedule.to_string().ends_with("(permanent error)"));
}

/// Deferring out of skipHours is noted only when it moved the time.
#[test]
fn test_plan_skip_deferral_noted() {
    // 21:39 falls in hour 21; skip hours 21 and 22.
    let skip_hours = (1 << 21) | (1 << 22);
    let schedule = plan(success_with_hint(None)).defer_past_skipped(skip_hours, 0);
    assert!(schedule.deferred_for_skip);
    // 23:00:00Z, the top of the first allowed hour.
    assert_eq!(schedule.next_fetch_at, PLAN_NOW + 4 * 3600 + 20 * 60 + 18);
    assert!(schedule
        .to_string()
        .contains("deferred past the feed's skipHours/skipDays"));

    let schedule = plan(success_with_hint(None)).defer_past_skipped(1 << 3, 0);
    assert!(!schedule.deferred_for_skip);
    assert_eq!(schedule.next_fetch_at, PLAN_NOW + MIN_FETCH_INTERVAL as i64);
}

#[test]
fn test_format_duration() {
    assert_eq!(format_duration(0), "0s");
    assert_eq!(format_duration(45), "45s");
    assert_eq!(format_duration(60), "1m");
    assert_eq!(format_duration(90), "1m 30s");
    assert_eq!(format_duration(10_800), "3h");
    assert_eq!(format_duration(7_500), "2h 5m");
    assert_eq!(format_duration(108_000), "1d 6h");
    assert_eq!(format_duration(86_430), "1d");
}
