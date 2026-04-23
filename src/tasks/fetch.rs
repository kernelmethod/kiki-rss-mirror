use crate::http::{FeedAuth, FeedAuthType};
use crate::metrics::Metrics;
use crate::scripting::ScriptRunner;
use crate::tasks::backoff::{
    compute_next_fetch_at, parse_retry_after, same_origin, FetchOutcome, SchedulerConfig,
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
use reqwest::Url;
use std::time::{Duration, Instant};
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
            auth_bearer_token
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
            })
        },
    )?;
    Ok(row)
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

    let conn = pool.get()?;
    let row = load_feed_fetch_row(&conn, feed_id)?;

    let cfg = SchedulerConfig {
        min_cadence: crate::db::settings::get_min_polling_cadence_seconds(&conn)?,
        max_backoff: crate::db::settings::get_max_feed_backoff_seconds(&conn)?,
        min_fetch_interval: row.min_fetch_interval.max(0) as u64,
        force_refresh_after: crate::db::settings::get_force_refresh_after_secs(&conn)?,
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
            metrics.record_feed_fetch("cache_hit", fetch_start.elapsed().as_secs_f64());
            return Ok(());
        }
    }

    // Handle file:// URLs differently
    let feed_content = if row.url.starts_with("file://") {
        retrieve_file_feed(&row.url, feed_id, pool.clone(), cfg)
    } else {
        retrieve_feed(
            client,
            feed_id,
            &row.url,
            row.header_etag.as_deref(),
            row.header_last_modified.as_deref(),
            row.header_immutable_until,
            row.header_body_hash.as_deref(),
            force_conditionals_off,
            &row.auth,
            pool.clone(),
            fetch_start,
            cfg,
            metrics,
            script_runner,
        )
        .await
    };

    let content = match feed_content {
        Ok(Some(c)) => c,
        // `retrieve_feed` / `retrieve_file_feed` already recorded the outcome
        // (e.g. 304 not-modified, HTTP error, skipped file) — nothing to parse.
        Ok(None) => return Ok(()),
        Err(e) => {
            metrics.record_feed_fetch("other", fetch_start.elapsed().as_secs_f64());
            return Err(e);
        }
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
            url: row.url.clone(),
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
        fire_fetch_error(
            script_runner,
            feed_id,
            "parse",
            None,
            format!("{}", fetch_err),
            None,
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
    stored_etag: Option<&str>,
    stored_last_modified: Option<&str>,
    immutable_until: Option<i64>,
    stored_body_hash: Option<&str>,
    force_conditionals_off: bool,
    auth: &FeedAuth,
    pool: Pool<SqliteConnectionManager>,
    fetch_start: Instant,
    cfg: SchedulerConfig,
    metrics: &Metrics,
    script_runner: Option<&dyn ScriptRunner>,
) -> Result<Option<Vec<u8>>> {
    let conn = pool.get()?;

    let timeout = Duration::from_secs(crate::db::settings::get_feed_update_timeout_seconds(&conn)?);

    // RFC 8246: while a prior response advertised `immutable` and is still
    // fresh, suppress conditional revalidation — the server has promised
    // the representation won't change. This also makes forced refresh a
    // no-op inside the immutable window (no conditionals to suppress).
    let skip_conditionals = immutable_until
        .map(|until| Utc::now().timestamp() < until)
        .unwrap_or(false);

    let mut current_url = feed_url.to_string();
    let mut had_permanent_redirect = false;
    let mut redirects: u64 = 0;
    let max_redirects = 10;

    let resp = 'redirect: {
        for _ in 0..=max_redirects {
            // Only send conditional headers on the first request. Also
            // suppress them when a forced (non-conditional) refresh is due,
            // so the body-hash comparison below can tell us whether the
            // server's validators have been honest.
            let mut request = client.get(&current_url).timeout(timeout);
            if current_url == feed_url && !skip_conditionals && !force_conditionals_off {
                if let Some(etag) = stored_etag {
                    request = request.header("If-None-Match", etag);
                }
                if let Some(last_modified) = stored_last_modified {
                    request = request.header("If-Modified-Since", last_modified);
                }
            }
            // Only forward credentials to the feed's configured origin. If a
            // redirect took us cross-origin we intentionally drop them to
            // avoid leaking secrets to an unrelated host.
            if same_origin(feed_url, &current_url) {
                request = auth.apply(request);
            } else if auth.auth_type != FeedAuthType::None {
                debug!(
                    "Feed {}: dropping auth on cross-origin redirect to {}",
                    feed_id, current_url
                );
            }

            let resp = match request.send().await {
                Ok(r) => r,
                Err(e) => {
                    let is_timeout = e.is_timeout();
                    let metric_outcome = if is_timeout { "timeout" } else { "other" };
                    let event_kind = if is_timeout { "timeout" } else { "network" };
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
                    fire_fetch_error(
                        script_runner,
                        feed_id,
                        event_kind,
                        None,
                        format!("{}", e),
                        None,
                    );
                    metrics.record_feed_fetch(metric_outcome, fetch_start.elapsed().as_secs_f64());
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
        fire_fetch_error(
            script_runner,
            feed_id,
            "too_many_redirects",
            None,
            format!("{}", fetch_err),
            None,
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
            fire_fetch_error(
                script_runner,
                feed_id,
                "http",
                Some(status_u16),
                format!("{}", fetch_err),
                retry_after_ts,
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

    // Update the feed's headers in the database. Capture every header-
    // derived value as owned data up front so we can consume `resp` for
    // the body below without fighting the borrow checker.
    let mut etag: Option<String> = resp
        .headers()
        .get("etag")
        .and_then(|h| h.to_str().ok())
        .map(str::to_string);
    let mut last_modified: Option<String> = resp
        .headers()
        .get("last-modified")
        .and_then(|h| h.to_str().ok())
        .map(str::to_string);

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
    let mut body_hash: Option<String>;
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

    // Read the body now so we can hash it before persisting. Done before
    // the UPDATE so a body-read failure leaves `feeds` untouched — we
    // don't advertise a successful fetch we couldn't actually read.
    let content = resp.bytes().await?;
    metrics.record_feed_response_bytes(content.len() as u64);
    body_hash = Some(blake3::hash(&content).to_hex().to_string());

    // Don't persist a body-hash fingerprint for responses we've been told
    // not to store (RFC 9111 §5.2.2 no-store).
    if cc.no_store {
        body_hash = None;
    }

    // Validator-lie detection. Only meaningful on a forced (non-
    // conditional) refresh, since otherwise the server is free to return
    // a body conditional on the validators we sent. A genuine content
    // change also rotates `ETag` and/or `Last-Modified`; if the body
    // differs from what we stored but neither validator budged, the
    // server has been serving 304s (or stale validators) while the
    // representation actually changed.
    if force_conditionals_off {
        let etag_unchanged = match (stored_etag, etag.as_deref()) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        };
        let lm_unchanged = match (stored_last_modified, last_modified.as_deref()) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        };
        let validators_unchanged = etag_unchanged || lm_unchanged;
        let body_changed = match (stored_body_hash, body_hash.as_deref()) {
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
            retry_after_at = NULL
         WHERE id = ?",
        (
            etag.as_deref(),
            last_modified.as_deref(),
            expires,
            immutable_until,
            body_hash.as_deref(),
            now_ts,
            now_ts,
            next_fetch_at,
            feed_id,
        ),
    )?;

    metrics.record_feed_retry_scheduled("cache_hint", (next_fetch_at - now_ts) as f64);

    fire_fetch_success(
        script_runner,
        feed_id,
        200,
        current_url.clone(),
        Some(content.len() as u64),
    );

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
