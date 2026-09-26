//! Unit tests for the response-header helpers in `tasks::cache`:
//! HTTP-date parsing, the RFC 9111 §4.2.3 age correction, and the
//! freshness hints derived from a full set of headers.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::super::cache::{corrected_max_age, extract_server_hints, parse_http_date};
use reqwest::header::{HeaderMap, HeaderValue};

/// A fixed "now" for the tests: Sun, 06 Nov 1994 08:49:37 GMT.
const NOW: i64 = 784_111_777;

fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.append(*name, HeaderValue::from_str(value).unwrap());
    }
    map
}

// ----------------- parse_http_date -----------------

/// All three HTTP-date formats from RFC 9110 §5.6.7 name the same instant.
#[test]
fn test_parse_http_date_all_three_formats() {
    let imf = parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT");
    let rfc850 = parse_http_date("Sunday, 06-Nov-94 08:49:37 GMT");
    let asctime = parse_http_date("Sun Nov  6 08:49:37 1994");
    assert_eq!(imf.map(|d| d.timestamp()), Some(NOW));
    assert_eq!(rfc850, imf);
    assert_eq!(asctime, imf);
}

/// asctime pads single-digit days with a space; two-digit days fill it.
#[test]
fn test_parse_http_date_asctime_two_digit_day() {
    let dt = parse_http_date("Thu Nov 17 08:49:37 1994").expect("asctime should parse");
    assert_eq!(dt.timestamp(), NOW + 11 * 86_400);
}

/// Surrounding whitespace is tolerated.
#[test]
fn test_parse_http_date_trims_whitespace() {
    assert_eq!(
        parse_http_date("  Sun, 06 Nov 1994 08:49:37 GMT \t").map(|d| d.timestamp()),
        Some(NOW)
    );
}

/// Invalid values, including the common `Expires: 0` and `-1`, don't parse.
#[test]
fn test_parse_http_date_invalid_values() {
    for value in [
        "0",
        "-1",
        "",
        "tomorrow",
        "Sun, 06 Nov 1994 08:49:37 PST",
        "1994-11-06T08:49:37Z",
    ] {
        assert!(
            parse_http_date(value).is_none(),
            "{value:?} should not parse"
        );
    }
}

// ----------------- corrected_max_age -----------------

/// With neither `Age` nor `Date`, `max-age` is returned unchanged.
#[test]
fn test_corrected_max_age_without_age_or_date() {
    assert_eq!(corrected_max_age(&HeaderMap::new(), 600, NOW), 600);
}

/// A `Date` in the future (server clock ahead of ours) must not extend
/// freshness past `max-age`; the apparent age clamps to zero.
#[test]
fn test_corrected_max_age_future_date_is_clamped() {
    let h = headers(&[("date", "Sun, 06 Nov 1994 09:49:37 GMT")]);
    assert_eq!(corrected_max_age(&h, 600, NOW), 600);
}

/// When both are present, the larger of `Age` and the `Date` delta wins.
#[test]
fn test_corrected_max_age_uses_larger_of_age_and_date() {
    // Date is 60s old; Age says 120s.
    let h = headers(&[("date", "Sun, 06 Nov 1994 08:48:37 GMT"), ("age", "120")]);
    assert_eq!(corrected_max_age(&h, 600, NOW), 480);
    // Date is 300s old; Age says 120s.
    let h = headers(&[("date", "Sun, 06 Nov 1994 08:44:37 GMT"), ("age", "120")]);
    assert_eq!(corrected_max_age(&h, 600, NOW), 300);
}

/// An upstream age beyond `max-age` saturates at zero rather than wrapping.
#[test]
fn test_corrected_max_age_saturates_at_zero() {
    let h = headers(&[("age", "10000")]);
    assert_eq!(corrected_max_age(&h, 600, NOW), 0);
}

/// Unparseable `Age` and `Date` values contribute nothing.
#[test]
fn test_corrected_max_age_ignores_garbage() {
    let h = headers(&[("age", "old"), ("date", "yesterday")]);
    assert_eq!(corrected_max_age(&h, 600, NOW), 600);
}

// ----------------- extract_server_hints -----------------

/// `immutable` without a `max-age` is recorded but gives no freshness
/// lifetime, so there is nothing for an immutable window to cover.
#[test]
fn test_hints_immutable_without_max_age() {
    let hints = extract_server_hints(&headers(&[("cache-control", "immutable")]), NOW);
    assert!(hints.immutable);
    assert_eq!(hints.hint_secs, None);
}

/// `Expires` in asctime format yields a hint.
#[test]
fn test_hints_expires_asctime() {
    let h = headers(&[("expires", "Sun Nov  6 09:49:37 1994")]);
    assert_eq!(extract_server_hints(&h, NOW).hint_secs, Some(3600));
}

/// An `Expires` in the past, or exactly now, gives no freshness and so no
/// hint, the same as an invalid one.
#[test]
fn test_hints_expires_in_past_gives_no_hint() {
    for value in [
        "Sun, 06 Nov 1994 07:49:37 GMT",
        "Sun, 06 Nov 1994 08:49:37 GMT",
    ] {
        let h = headers(&[("expires", value)]);
        assert_eq!(
            extract_server_hints(&h, NOW).hint_secs,
            None,
            "Expires: {value}"
        );
    }
}

/// An invalid `Expires` such as `0` gives no freshness (RFC 9111 §5.3
/// says to treat it as already expired), so no hint is produced.
#[test]
fn test_hints_invalid_expires_gives_no_hint() {
    for value in ["0", "-1", "never"] {
        let h = headers(&[("expires", value)]);
        assert_eq!(
            extract_server_hints(&h, NOW).hint_secs,
            None,
            "Expires: {value}"
        );
    }
}

/// `max-age` takes precedence over an invalid `Expires`.
#[test]
fn test_hints_max_age_wins_over_invalid_expires() {
    let h = headers(&[("cache-control", "max-age=300"), ("expires", "0")]);
    assert_eq!(extract_server_hints(&h, NOW).hint_secs, Some(300));
}

/// A future `Date` doesn't stretch `max-age` in the derived hint either.
#[test]
fn test_hints_future_date_does_not_extend_max_age() {
    let h = headers(&[
        ("cache-control", "max-age=600"),
        ("date", "Sun, 06 Nov 1994 10:49:37 GMT"),
    ]);
    assert_eq!(extract_server_hints(&h, NOW).hint_secs, Some(600));
}

/// `stale-if-error` is carried through even when the response has no
/// freshness lifetime of its own.
#[test]
fn test_hints_stale_if_error_without_max_age() {
    let h = headers(&[("cache-control", "no-cache, stale-if-error=1800")]);
    let hints = extract_server_hints(&h, NOW);
    assert_eq!(hints.stale_if_error, Some(1800));
    assert_eq!(hints.hint_secs, None);
}
