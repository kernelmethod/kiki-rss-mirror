use crate::config::Settings;
use crate::fetcher::{
    FeedHints, FetchReply, FetchSpec, FetchedBody, Fetcher, ParseOutcome, ParsedFeed,
};
use crate::http::{FeedAuth, FeedAuthType};
use crate::metrics::Metrics;
use crate::scripting::ScriptRunner;
use crate::tasks::backoff::{
    compute_next_fetch_at, defer_past_skipped, parse_retry_after, FetchOutcome, SchedulerConfig,
};
use crate::tasks::cache::{corrected_max_age, extract_server_hints, parse_http_date, CacheControl};
use crate::tasks::command::TaskManagerCommand;
use crate::tasks::error::FetchError;
use crate::tasks::error_recording::{clear_feed_error, set_feed_error_with_schedule};
use crate::tasks::processing::{enqueue_asset_caching, process_atom_feed, process_rss_feed};
use crate::tasks::scripting::{fire_fetch_error, fire_fetch_success};
use anyhow::Result;
use chrono::Utc;
use r2d2::{Pool, PooledConnection};
use r2d2_sqlite::SqliteConnectionManager;
use std::time::Instant;
use tracing::{debug, info, warn};

/// Snapshot of a `feeds` row loaded at the start of a refresh: the URL,
/// cache validators, body fingerprint, and scheduling state that drive the
/// fetch decision.
struct FeedFetchRow {
    url: String,
    header_etag: Option<String>,
    header_last_modified: Option<String>,
    header_immutable_until: Option<i64>,
    header_body_hash: Option<String>,
    last_full_refresh_at: Option<i64>,
    consecutive_failures: i64,
    next_fetch_at: Option<i64>,
    min_fetch_interval: i64,
    auth: FeedAuth,
    /// Refresh hints from the last feed document that parsed, used when a
    /// response carries no body to read them from (a 304).
    feed_hints: FeedHints,
}

fn load_feed_fetch_row(
    conn: &PooledConnection<SqliteConnectionManager>,
    feed_id: i64,
) -> Result<FeedFetchRow> {
    let row = conn.query_row(
        "SELECT
            url,
            header_etag,
            header_last_modified,
            header_immutable_until,
            header_body_hash,
            last_full_refresh_at,
            consecutive_failures,
            next_fetch_at,
            min_fetch_interval_seconds,
            auth_type,
            auth_username,
            auth_password,
            auth_bearer_token,
            feed_ttl_seconds,
            feed_update_interval_seconds,
            feed_skip_hours,
            feed_skip_days
         FROM feeds
         WHERE id = ?1",
        [feed_id],
        |row| {
            let auth_type_raw: Option<String> = row.get(9)?;
            let auth_type = FeedAuthType::from_db(auth_type_raw.as_deref()).unwrap_or_else(|e| {
                warn!(
                    "Feed {}: invalid auth_type in database: {}; treating as 'none'",
                    feed_id, e
                );
                FeedAuthType::None
            });
            Ok(FeedFetchRow {
                url: row.get(0)?,
                header_etag: row.get(1)?,
                header_last_modified: row.get(2)?,
                header_immutable_until: row.get(3)?,
                header_body_hash: row.get(4)?,
                last_full_refresh_at: row.get(5)?,
                consecutive_failures: row.get(6)?,
                next_fetch_at: row.get(7)?,
                min_fetch_interval: row.get(8)?,
                auth: FeedAuth {
                    auth_type,
                    username: row.get(10)?,
                    password: row.get(11)?,
                    bearer_token: row.get(12)?,
                },
                feed_hints: FeedHints {
                    ttl_secs: row.get::<_, Option<i64>>(13)?.map(|v| v.max(0) as u64),
                    update_interval_secs: row.get::<_, Option<i64>>(14)?.map(|v| v.max(0) as u64),
                    skip_hours: row.get::<_, i64>(15)? as u32,
                    skip_days: row.get::<_, i64>(16)? as u8,
                },
            })
        },
    )?;
    Ok(row)
}

/// Refresh the feed corresponding to the provided `feed_id`.
///
/// Reads what the fetch needs out of the database, hands it to `fetcher`
/// — which may be another process — and records whatever comes back.
/// Every database write, script event, and metric happens here, in the
/// caller's process; the fetcher only ever sees a [`FetchSpec`].
pub(crate) async fn refresh_feed(
    fetcher: &Fetcher,
    feed_id: i64,
    pool: Pool<SqliteConnectionManager>,
    settings: &Settings,
    script_runner: Option<&dyn ScriptRunner>,
    metrics: &Metrics,
    task_tx: &async_channel::Sender<TaskManagerCommand>,
) -> Result<()> {
    let fetch_start = Instant::now();
    let fetch_settings = &settings.feed_fetch;

    let conn = pool.get()?;
    let row = load_feed_fetch_row(&conn, feed_id)?;

    let cfg = SchedulerConfig {
        min_cadence: fetch_settings.min_polling_cadence_seconds,
        max_backoff: fetch_settings.max_backoff_seconds,
        min_fetch_interval: row.min_fetch_interval.max(0) as u64,
        force_refresh_after: fetch_settings.force_refresh_after_seconds,
    };
    let rec = Recorder {
        pool: &pool,
        feed_id,
        cfg,
        max_feed_bytes: fetch_settings.max_feed_bytes,
        fetch_start,
        metrics,
        script_runner,
    };

    // Force a non-conditional refresh when enough time has passed since the
    // last full 200, and only when the feed isn't currently in a failure
    // streak (don't add bandwidth cost to a feed that's already struggling).
    // First-time fetches are always full, so `None` just means "not yet,
    // keep using conditionals once we have validators".
    let now_ts = Utc::now().timestamp();
    let force_conditionals_off = row.last_full_refresh_at.is_some_and(|t| {
        row.consecutive_failures == 0 && now_ts.saturating_sub(t) as u64 >= cfg.force_refresh_after
    });

    // Eligibility gate: a scheduled `next_fetch_at` in the future means skip.
    if let Some(next_ts) = row.next_fetch_at {
        if now_ts < next_ts {
            debug!(
                "Feed {} not yet eligible; next fetch in {} seconds",
                feed_id,
                next_ts - now_ts
            );
            metrics.record_feed_cache_hit("next_fetch_at");
            rec.outcome("cache_hit");
            return Ok(());
        }
    }

    // file:// feeds are read here, because the fetcher has no filesystem
    // access, and only the parse is handed off.
    let fetched = if row.url.starts_with("file://") {
        match retrieve_file_feed(&row.url, feed_id, pool.clone(), cfg) {
            Ok(content) => fetcher.parse(feed_id, content).await.map(Some),
            Err(e) => {
                rec.outcome("other");
                return Err(e);
            }
        }
    } else {
        // RFC 8246: while a prior response advertised `immutable` and is
        // still fresh, suppress conditional revalidation — the server has
        // promised the representation won't change. This also makes forced
        // refresh a no-op inside the immutable window (no conditionals to
        // suppress).
        let skip_conditionals = row
            .header_immutable_until
            .map(|until| Utc::now().timestamp() < until)
            .unwrap_or(false);

        let spec = FetchSpec {
            feed_id,
            url: row.url.clone(),
            etag: row.header_etag.clone(),
            last_modified: row.header_last_modified.clone(),
            // Also suppressed when a forced (non-conditional) refresh is
            // due, so the body-hash comparison can tell us whether the
            // server's validators have been honest.
            send_conditionals: !skip_conditionals && !force_conditionals_off,
            auth: row.auth.clone(),
            // Both taken from the settings snapshot for this fetch, so an
            // operator changing them does not have to restart the server
            // for it to take effect.
            timeout_secs: fetch_settings.timeout_seconds,
            max_feed_bytes: fetch_settings.max_feed_bytes,
        };
        match fetcher.fetch(spec).await {
            Ok(reply) => match record_fetch_reply(&rec, &row, reply, force_conditionals_off) {
                Ok(parsed) => Ok(parsed),
                Err(e) => {
                    rec.outcome("other");
                    return Err(e);
                }
            },
            Err(e) => Err(e),
        }
    };

    let parsed = match fetched {
        Ok(Some(parsed)) => parsed,
        // The outcome (304, HTTP error, oversized body, ...) has been
        // recorded and there is nothing to store.
        Ok(None) => return Ok(()),
        // The fetcher itself failed, not the feed server. Recorded as a
        // transient error so the feed backs off: if its content is what
        // crashed the fetcher, retrying on the next tick would only crash
        // it again.
        Err(e) => {
            let message = format!("{}", e);
            rec.fail(FetchError::Other { message }, "fetcher", "fetcher_error");
            return Ok(());
        }
    };

    let Some(feed) = parsed.feed else {
        let url = row.url.clone();
        rec.fail(FetchError::InvalidFeed { url }, "parse", "invalid_feed");
        return Ok(());
    };
    metrics.record_feed_parse(feed.format(), parsed.seconds, feed.entry_count() as u64);
    let inserted = match feed {
        ParsedFeed::Atom { feed, entries, .. } => {
            process_atom_feed(feed_id, *feed, entries, pool.get()?, script_runner, metrics)?
        }
        ParsedFeed::Rss { entries, .. } => {
            process_rss_feed(feed_id, entries, pool.get()?, script_runner, metrics)?
        }
    };
    enqueue_asset_caching(task_tx, metrics, &inserted);
    clear_feed_error(&conn, feed_id);
    rec.outcome("success");
    Ok(())
}

/// Everything the outcome of one refresh is recorded against.
struct Recorder<'a> {
    pool: &'a Pool<SqliteConnectionManager>,
    feed_id: i64,
    cfg: SchedulerConfig,
    /// The body-size cap this fetch was made with.
    max_feed_bytes: u64,
    fetch_start: Instant,
    metrics: &'a Metrics,
    script_runner: Option<&'a dyn ScriptRunner>,
}

impl Recorder<'_> {
    /// Count the refresh under `outcome` in the fetch metrics.
    fn outcome(&self, outcome: &'static str) {
        self.metrics
            .record_feed_fetch(outcome, self.fetch_start.elapsed().as_secs_f64());
    }

    /// Record a failed fetch with no server-supplied hints.
    fn fail(&self, err: FetchError, kind: &'static str, outcome: &'static str) {
        self.fail_with(err, kind, outcome, None, None, None);
    }

    /// Record a failed fetch: store `err` against the feed and reschedule
    /// it, fire `fetch.error` with `kind`, and count it under `outcome`.
    fn fail_with(
        &self,
        err: FetchError,
        kind: &'static str,
        outcome: &'static str,
        status: Option<u16>,
        retry_after_ts: Option<i64>,
        stale_if_error: Option<u64>,
    ) {
        warn!("Feed {}: {}", self.feed_id, err);
        match self.pool.get() {
            Ok(conn) => set_feed_error_with_schedule(
                &conn,
                self.feed_id,
                &err,
                retry_after_ts,
                stale_if_error,
                self.cfg.min_cadence,
                self.cfg.max_backoff,
                self.cfg.min_fetch_interval,
                self.metrics,
            ),
            Err(e) => warn!("Feed {}: could not record the error: {}", self.feed_id, e),
        }
        fire_fetch_error(
            self.script_runner,
            self.feed_id,
            kind,
            status,
            format!("{}", err),
            retry_after_ts,
        );
        self.outcome(outcome);
    }
}

/// Write the outcome of an HTTP fetch into the `feeds` row, fire the
/// matching script event, and record metrics.
///
/// Returns the parse result when the server sent a `200 OK` whose body was
/// read, and `None` for every outcome that leaves nothing to store.
///
/// # Errors
///
/// Returns an error for [`FetchReply::Failed`] and for database failures;
/// the caller records those as an unscheduled error.
fn record_fetch_reply(
    rec: &Recorder,
    row: &FeedFetchRow,
    reply: FetchReply,
    force_conditionals_off: bool,
) -> Result<Option<ParseOutcome>> {
    let (feed_id, cfg, metrics) = (rec.feed_id, rec.cfg, rec.metrics);
    let conn = rec.pool.get()?;
    let feed_url = row.url.as_str();

    let body = match reply {
        FetchReply::Failed { message } => return Err(anyhow::anyhow!(message)),

        FetchReply::Network {
            message,
            timeout,
            redirects,
        } => {
            metrics.record_feed_redirects(redirects);
            let (kind, outcome) = if timeout {
                ("timeout", "timeout")
            } else {
                ("network", "other")
            };
            rec.fail(FetchError::Other { message }, kind, outcome);
            return Ok(None);
        }

        FetchReply::TooManyRedirects { redirects } => {
            metrics.record_feed_redirects(redirects);
            let url = feed_url.to_string();
            let kind = "too_many_redirects";
            rec.fail(FetchError::TooManyRedirects { url }, kind, kind);
            return Ok(None);
        }

        FetchReply::NotModified { headers, redirects } => {
            metrics.record_feed_redirects(redirects);
            info!("Feed {} was not modified since last check", feed_id);
            let now_ts = Utc::now().timestamp();
            let hints = extract_server_hints(&headers.to_header_map(), now_ts);
            let next_fetch_at = schedule_success(
                FetchOutcome::NotModified {
                    server_hint_secs: hints.hint_secs.or(row.feed_hints.refresh_hint_secs()),
                },
                &row.feed_hints,
                now_ts,
                cfg,
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
            rec.outcome("not_modified");
            return Ok(None);
        }

        FetchReply::HttpStatus {
            status,
            headers,
            redirects,
        } => {
            metrics.record_feed_redirects(redirects);
            let headers = headers.to_header_map();

            // Parse Retry-After for 429 Too Many Requests and 503 Service
            // Unavailable. Other statuses don't carry retry hints.
            let retry_after_ts = if status == 429 || status == 503 {
                headers
                    .get("retry-after")
                    .and_then(|h| h.to_str().ok())
                    .and_then(|s| parse_retry_after(s, Utc::now()))
            } else {
                None
            };

            // Honor Cache-Control: stale-if-error on the error response
            // (RFC 5861 §4) so our backoff doesn't outlive the grace window.
            let hints = extract_server_hints(&headers, Utc::now().timestamp());

            let url = feed_url.to_string();
            rec.fail_with(
                FetchError::HttpStatus { url, status },
                "http",
                "http_error",
                Some(status),
                retry_after_ts,
                hints.stale_if_error,
            );
            return Ok(None);
        }

        FetchReply::BodyTooLarge {
            final_url,
            seen,
            redirects,
        } => {
            metrics.record_feed_redirects(redirects);
            debug!("Feed {}: gave up on the body after {} bytes", feed_id, seen);
            let limit = rec.max_feed_bytes;
            let url = final_url;
            let kind = "body_too_large";
            rec.fail(FetchError::BodyTooLarge { url, limit }, kind, kind);
            return Ok(None);
        }

        FetchReply::Body(body) => body,
    };

    let FetchedBody {
        final_url,
        permanent_redirect,
        redirects,
        headers,
        body_len,
        body_hash,
        parsed,
    } = *body;
    metrics.record_feed_redirects(redirects);
    let headers = headers.to_header_map();

    // If we followed a permanent redirect, update the stored URL in the database
    if permanent_redirect && final_url != feed_url {
        info!(
            "Feed {} permanently redirected from {} to {}; updating stored URL",
            feed_id, feed_url, final_url
        );
        conn.execute(
            "UPDATE feeds SET url = ?1 WHERE id = ?2",
            (&final_url, feed_id),
        )?;
    }

    let mut etag: Option<String> = headers
        .get("etag")
        .and_then(|h| h.to_str().ok())
        .map(str::to_string);
    let mut last_modified: Option<String> = headers
        .get("last-modified")
        .and_then(|h| h.to_str().ok())
        .map(str::to_string);

    // Parse the Expires header (RFC 9111 §5.3) into a Unix timestamp so we
    // can skip future fetches until the declared expiry time has passed.
    // Accepts all three HTTP-date formats (RFC 9110 §5.6.7).
    let mut expires: Option<i64> = headers
        .get("expires")
        .and_then(|h| h.to_str().ok())
        .and_then(parse_http_date)
        .map(|dt| dt.timestamp());

    let now_ts = Utc::now().timestamp();

    // Derive freshness hints before Cache-Control directives mutate
    // `expires` — the hint reflects the server's original instruction.
    let hints = extract_server_hints(&headers, now_ts);

    // Parse Cache-Control and apply precedence rules (RFC 9111 §5.2):
    // - no-store: clear all cache headers
    // - no-cache: allow conditional requests but never skip fetching
    // - max-age: overrides Expires header (adjusted for upstream age per
    //   RFC 9111 §4.2.3)
    let cc_values: Vec<&str> = headers
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
        let corrected = corrected_max_age(&headers, max_age, now_ts);
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

    // Refresh hints from the feed document. A body that did not parse
    // leaves the previously stored ones in force.
    let feed_hints = parsed
        .feed
        .as_ref()
        .map_or(row.feed_hints, |feed| *feed.hints());

    // HTTP freshness takes precedence (RFC 9111 is specific to this
    // representation); the feed's own <ttl> / sy:update* is the fallback.
    let next_fetch_at = schedule_success(
        FetchOutcome::Success {
            server_hint_secs: hints.hint_secs.or(feed_hints.refresh_hint_secs()),
        },
        &feed_hints,
        now_ts,
        cfg,
    );

    metrics.record_feed_response_bytes(body_len);

    // Don't persist a body-hash fingerprint for responses we've been told
    // not to store (RFC 9111 §5.2.2 no-store).
    let body_hash: Option<String> = if cc.no_store { None } else { Some(body_hash) };

    // Validator-lie detection. Only meaningful on a forced (non-
    // conditional) refresh, since otherwise the server is free to return
    // a body conditional on the validators we sent. A genuine content
    // change also rotates `ETag` and/or `Last-Modified`; if the body
    // differs from what we stored but neither validator budged, the
    // server has been serving 304s (or stale validators) while the
    // representation actually changed.
    if force_conditionals_off {
        let etag_unchanged = match (row.header_etag.as_deref(), etag.as_deref()) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        };
        let lm_unchanged = match (
            row.header_last_modified.as_deref(),
            last_modified.as_deref(),
        ) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        };
        let validators_unchanged = etag_unchanged || lm_unchanged;
        let body_changed = match (row.header_body_hash.as_deref(), body_hash.as_deref()) {
            (Some(stored), Some(fresh)) => stored != fresh,
            _ => false,
        };

        if validators_unchanged && body_changed {
            warn!(
                "Feed {} ({}): server returned unchanged ETag/Last-Modified but body hash differs; validators appear untrustworthy",
                feed_id, feed_url
            );
            metrics.record_feed_validator_lie();
        }
        metrics.record_feed_forced_refresh(if body_changed { "mismatch" } else { "match" });
    }

    conn.execute(
        "UPDATE feeds SET
            header_etag = ?,
            header_last_modified = ?,
            header_expires = ?,
            header_immutable_until = ?,
            header_body_hash = ?,
            last_full_refresh_at = ?,
            last_checked = ?,
            next_fetch_at = ?,
            consecutive_failures = 0,
            retry_after_at = NULL,
            feed_ttl_seconds = ?,
            feed_update_interval_seconds = ?,
            feed_skip_hours = ?,
            feed_skip_days = ?
         WHERE id = ?",
        rusqlite::params![
            etag.as_deref(),
            last_modified.as_deref(),
            expires,
            immutable_until,
            body_hash.as_deref(),
            now_ts,
            now_ts,
            next_fetch_at,
            feed_hints.ttl_secs.map(saturating_i64),
            feed_hints.update_interval_secs.map(saturating_i64),
            feed_hints.skip_hours,
            feed_hints.skip_days,
            feed_id,
        ],
    )?;

    metrics.record_feed_retry_scheduled("cache_hint", (next_fetch_at - now_ts) as f64);

    fire_fetch_success(rec.script_runner, feed_id, 200, final_url, Some(body_len));

    Ok(Some(parsed))
}

/// Convert seconds to SQLite's integer type; an absurd `<ttl>` saturates
/// rather than wrapping negative.
fn saturating_i64(secs: u64) -> i64 {
    i64::try_from(secs).unwrap_or(i64::MAX)
}

/// Schedule the next fetch after a 200 or 304, then move it out of any
/// hours or days the feed asked not to be read in.
fn schedule_success(
    outcome: FetchOutcome,
    feed_hints: &FeedHints,
    now_ts: i64,
    cfg: SchedulerConfig,
) -> i64 {
    let next_fetch_at = compute_next_fetch_at(
        outcome,
        now_ts,
        cfg.min_cadence,
        cfg.max_backoff,
        cfg.min_fetch_interval,
    );
    defer_past_skipped(next_fetch_at, feed_hints.skip_hours, feed_hints.skip_days)
}

fn retrieve_file_feed(
    feed_url: &str,
    feed_id: i64,
    pool: Pool<SqliteConnectionManager>,
    cfg: SchedulerConfig,
) -> Result<Vec<u8>> {
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

    Ok(content)
}
