//! HTTP response cache-header parsing for the feed fetcher.
//!
//! Covers the subset of RFC 9111 / RFC 9110 / RFC 8246 / RFC 5861 that a
//! polling single-client cache needs: Cache-Control directives (`max-age`,
//! `no-cache`, `no-store`, `immutable`, `stale-if-error`), Age/Date-based
//! corrected-age adjustment, Expires parsing across all three HTTP-date
//! formats, and Pragma: no-cache as an HTTP/1.0 fallback.

use chrono::{DateTime, Utc};
use tracing::debug;

/// Parsed `Cache-Control` directives relevant to the fetcher.
///
/// Covers the subset we act on (see RFC 9111 §5.2 for the full directive list,
/// RFC 8246 for `immutable`, RFC 5861 §4 for `stale-if-error`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CacheControl {
    /// `max-age=N` — freshness lifetime in seconds (RFC 9111 §5.2.2.1).
    pub max_age: Option<u64>,
    /// `no-cache` — must revalidate; don't skip fetching (RFC 9111 §5.2.2.4).
    pub no_cache: bool,
    /// `no-store` — don't retain any stored response (RFC 9111 §5.2.2.5).
    pub no_store: bool,
    /// `immutable` — freshness is guaranteed for the declared `max-age`,
    /// so revalidation can be skipped while fresh (RFC 8246).
    pub immutable: bool,
    /// `stale-if-error=N` — allow serving the stored response for up to N
    /// seconds after it becomes stale if revalidation fails (RFC 5861 §4).
    pub stale_if_error: Option<u64>,
}

impl CacheControl {
    /// Parse a set of `Cache-Control` header values into the directives we
    /// care about.
    ///
    /// Combines multiple header instances per RFC 9110 §5.3 — directives
    /// from later values are OR'd / overwritten onto earlier ones. Unknown
    /// directives are ignored, and recognized directives with unparseable
    /// numeric values are logged at `debug` without aborting the parse.
    pub fn parse_many(values: &[&str]) -> Self {
        let mut cc = CacheControl::default();

        for header in values {
            for directive in header.split(',') {
                let directive = directive.trim();
                if directive.is_empty() {
                    continue;
                }

                // Split on the first `=`; anything after is the (optionally
                // quoted) argument.
                let (name, value) = match directive.split_once('=') {
                    Some((n, v)) => (n.trim(), Some(strip_quotes(v.trim()))),
                    None => (directive, None),
                };

                if name.eq_ignore_ascii_case("no-cache") {
                    cc.no_cache = true;
                } else if name.eq_ignore_ascii_case("no-store") {
                    cc.no_store = true;
                } else if name.eq_ignore_ascii_case("immutable") {
                    cc.immutable = true;
                } else if name.eq_ignore_ascii_case("max-age") {
                    match value.and_then(|v| v.parse::<u64>().ok()) {
                        Some(v) => cc.max_age = Some(v),
                        None => debug!(
                            directive = name,
                            value = value.unwrap_or(""),
                            "unparseable Cache-Control directive"
                        ),
                    }
                } else if name.eq_ignore_ascii_case("stale-if-error") {
                    match value.and_then(|v| v.parse::<u64>().ok()) {
                        Some(v) => cc.stale_if_error = Some(v),
                        None => debug!(
                            directive = name,
                            value = value.unwrap_or(""),
                            "unparseable Cache-Control directive"
                        ),
                    }
                }
            }
        }

        cc
    }
}

/// Strip one pair of surrounding double quotes from `s`, if present.
fn strip_quotes(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(s)
}

/// Parse an HTTP-date per RFC 9110 §5.6.7. Accepts IMF-fixdate
/// (preferred), obsolete RFC 850, and asctime formats. Returns `None`
/// if the value does not match any of the three.
pub fn parse_http_date(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();

    // IMF-fixdate, e.g. "Sun, 06 Nov 1994 08:49:37 GMT"
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%a, %d %b %Y %H:%M:%S GMT") {
        return Some(dt.and_utc());
    }
    // Obsolete RFC 850, e.g. "Sunday, 06-Nov-94 08:49:37 GMT"
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%A, %d-%b-%y %H:%M:%S GMT") {
        return Some(dt.and_utc());
    }
    // asctime, e.g. "Sun Nov  6 08:49:37 1994"
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%a %b %e %H:%M:%S %Y") {
        return Some(dt.and_utc());
    }
    None
}

/// Adjust `max_age` by subtracting the apparent upstream age of the response.
///
/// Implements the corrected-age calculation from RFC 9111 §4.2.3 / §5.1:
/// `age = max(Age_header, max(0, response_time - Date_header))`. Missing
/// headers contribute 0. The result is saturated at 0.
pub fn corrected_max_age(
    headers: &reqwest::header::HeaderMap,
    max_age: u64,
    response_time: i64,
) -> u64 {
    let age_header = headers
        .get("age")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);

    let date_delta = headers
        .get("date")
        .and_then(|h| h.to_str().ok())
        .and_then(parse_http_date)
        .map(|d| (response_time - d.timestamp()).max(0) as u64)
        .unwrap_or(0);

    let age = age_header.max(date_delta);
    max_age.saturating_sub(age)
}

/// Freshness-related hints extracted from a response's headers.
#[derive(Debug, Default, Clone, Copy)]
pub struct ServerHints {
    /// Freshness lifetime, in seconds from now. `None` means no hint.
    pub hint_secs: Option<u64>,
    /// `Cache-Control: stale-if-error=N` (RFC 5861 §4).
    pub stale_if_error: Option<u64>,
    /// `Cache-Control: immutable` (RFC 8246).
    pub immutable: bool,
}

/// Extract freshness/error hints from a response's headers.
///
/// `max-age` takes precedence over `Expires` (RFC 9111 §5.3). When
/// `max-age` is applied, the corrected-age adjustment from RFC 9111 §4.2.3
/// is subtracted so a server's `Age` / `Date` headers reduce the effective
/// freshness. An `Expires` that is in the past or invalid gives no hint.
/// If `cache-control` is absent entirely, an HTTP/1.0
/// `Pragma: no-cache` (RFC 9111 §5.4) clears any freshness hint.
pub fn extract_server_hints(resp_headers: &reqwest::header::HeaderMap, now_ts: i64) -> ServerHints {
    // RFC 9110 §5.3: multiple Cache-Control fields are combined.
    let cc_values: Vec<&str> = resp_headers
        .get_all("cache-control")
        .iter()
        .filter_map(|h| h.to_str().ok())
        .collect();
    let has_cc = !cc_values.is_empty();
    let cc = CacheControl::parse_many(&cc_values);

    let mut hint_secs: Option<u64> = None;

    if cc.no_store || cc.no_cache {
        // Don't set a freshness hint; conditional revalidation is still fine.
    } else if let Some(max_age) = cc.max_age {
        hint_secs = Some(corrected_max_age(resp_headers, max_age, now_ts));
    } else {
        // Fall back to Expires (RFC 9111 §5.3). An Expires that is already
        // in the past gives no freshness, the same as an invalid one such as
        // `0`, which RFC 9111 §5.3 says to treat as already expired. Neither
        // is a request to poll again right away, so both leave the feed on
        // its per-feed interval rather than the min-cadence floor.
        hint_secs = resp_headers
            .get("expires")
            .and_then(|h| h.to_str().ok())
            .and_then(parse_http_date)
            .and_then(|dt| u64::try_from(dt.timestamp() - now_ts).ok())
            .filter(|&secs| secs > 0);
    }

    if !has_cc {
        // RFC 9111 §5.4: treat Pragma: no-cache as no-cache when
        // Cache-Control is absent.
        let pragma_no_cache = resp_headers
            .get_all("pragma")
            .iter()
            .filter_map(|h| h.to_str().ok())
            .flat_map(|v| v.split(','))
            .any(|tok| tok.trim().eq_ignore_ascii_case("no-cache"));
        if pragma_no_cache {
            hint_secs = None;
        }
    }

    ServerHints {
        hint_secs,
        stale_if_error: cc.stale_if_error,
        immutable: cc.immutable,
    }
}
