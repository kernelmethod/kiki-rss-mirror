#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::super::*;

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
