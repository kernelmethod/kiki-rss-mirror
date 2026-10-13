//! The HTTP cache rules every outbound request follows.
//!
//! Requests are made by the fetcher, which may be another process with no
//! database, so using the HTTP cache ([`crate::db::http_cache`]) is split
//! in two around it, the way proxy settings are:
//!
//! 1. Before the request, [`request_validators`] turns what is stored for
//!    the URL into the [`Validators`] put in the request's spec, which the
//!    fetcher sends with [`Validators::apply`].
//! 2. After it, [`record_response`] stores what the response's headers say,
//!    whether it was a full response or a `304 Not Modified`.
//!
//! Underneath is header parsing covering the subset of RFC 9111 / RFC 9110
//! / RFC 8246 / RFC 5861 that a polling single-client cache needs:
//! Cache-Control directives (`max-age`, `no-cache`, `no-store`,
//! `immutable`, `stale-if-error`), Age/Date-based corrected-age adjustment,
//! Expires parsing across all three HTTP-date formats, and Pragma: no-cache
//! as an HTTP/1.0 fallback.

use crate::db::http_cache::{self, CacheEntry};
use crate::fetcher::Validators;
use anyhow::Result;
use chrono::{DateTime, Utc};
use reqwest::header::HeaderMap;
use rusqlite::Connection;
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

/// The validators to make a request conditional on, given the `entry`
/// stored for its URL, at Unix time `now_ts`.
///
/// None are sent on a `full` request, one that must fetch the whole body
/// whatever the server holds, or while an RFC 8246 `immutable` window is
/// open: the server has promised the representation won't change.
pub fn request_validators(entry: &CacheEntry, now_ts: i64, full: bool) -> Validators {
    let immutable = entry.immutable_until.is_some_and(|until| now_ts < until);
    if full || immutable {
        return Validators::default();
    }
    Validators {
        etag: entry.etag.clone(),
        last_modified: entry.last_modified.clone(),
    }
}

/// What kind of response is being recorded in the HTTP cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Response<'a> {
    /// A `200 OK` whose body was read, with its Blake3 hex digest.
    Full { body_hash: &'a str },
    /// A `304 Not Modified`.
    NotModified,
}

/// The cache entry to store after `response`, whose headers are `headers`,
/// for a URL that had `prev` stored, at Unix time `now_ts`.
///
/// Applies the Cache-Control precedence rules (RFC 9111 §5.2): `no-store`
/// clears the validators, expiry and body hash, `no-cache` clears the
/// expiry but keeps the validators, and `max-age` overrides `Expires` after
/// the RFC 9111 §4.2.3 age correction.
///
/// A 304 freshens the stored response instead (RFC 9111 §4.3.4): header
/// fields it carries replace the stored ones, and fields it omits keep
/// their stored values, as does the fingerprint of the last full body.
pub fn entry_after_response(
    prev: &CacheEntry,
    response: Response,
    headers: &HeaderMap,
    now_ts: i64,
) -> CacheEntry {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|h| h.to_str().ok())
            .map(str::to_string)
    };
    let cc_values: Vec<&str> = headers
        .get_all("cache-control")
        .iter()
        .filter_map(|h| h.to_str().ok())
        .collect();
    let cc = CacheControl::parse_many(&cc_values);

    // Accepts all three HTTP-date formats (RFC 9110 §5.6.7).
    let mut expires_at: Option<i64> = header("expires")
        .as_deref()
        .and_then(parse_http_date)
        .map(|dt| dt.timestamp());
    if cc.no_cache {
        expires_at = None;
    } else if let Some(max_age) = cc.max_age {
        let corrected = corrected_max_age(headers, max_age, now_ts);
        expires_at = Some(now_ts + corrected as i64);
    }

    // RFC 8246: while the response is fresh, skip conditional revalidation
    // entirely. Only meaningful when paired with a positive max-age.
    let hints = extract_server_hints(headers, now_ts);
    let immutable_until = hints
        .hint_secs
        .filter(|&s| hints.immutable && s > 0)
        .map(|s| now_ts.saturating_add(s as i64));

    let (body_hash, last_full_refresh_at) = match response {
        Response::Full { body_hash } => (Some(body_hash.to_string()), Some(now_ts)),
        Response::NotModified => (prev.body_hash.clone(), prev.last_full_refresh_at),
    };

    if cc.no_store {
        return CacheEntry {
            body_hash: match response {
                Response::Full { .. } => None,
                Response::NotModified => body_hash,
            },
            last_full_refresh_at,
            ..CacheEntry::default()
        };
    }

    let mut entry = CacheEntry {
        etag: header("etag"),
        last_modified: header("last-modified"),
        expires_at,
        immutable_until,
        body_hash,
        last_full_refresh_at,
    };
    if response == Response::NotModified {
        let carries_freshness =
            headers.contains_key("cache-control") || headers.contains_key("expires");
        entry.etag = entry.etag.or_else(|| prev.etag.clone());
        entry.last_modified = entry.last_modified.or_else(|| prev.last_modified.clone());
        if !carries_freshness {
            entry.expires_at = prev.expires_at;
            entry.immutable_until = prev.immutable_until;
        }
    }
    entry
}

/// Record `response` in the HTTP cache under `url`, which had `prev`
/// stored, and return the entry stored. See [`entry_after_response`].
///
/// # Errors
///
/// Returns an error if the write fails.
pub fn record_response(
    conn: &Connection,
    url: &str,
    prev: &CacheEntry,
    response: Response,
    headers: &HeaderMap,
    now_ts: i64,
) -> Result<CacheEntry> {
    let entry = entry_after_response(prev, response, headers, now_ts);
    http_cache::store(conn, url, &entry)?;
    Ok(entry)
}
