use crate::config::Settings;
use crate::db::feeds::merge_feed_into;
use crate::db::Db;
use crate::fetcher::{
    FeedHints, FetchReply, FetchSpec, FetchedBody, Fetcher, FetcherError, ParseOutcome, ParsedFeed,
};
use crate::http::{FeedAuth, FeedAuthType};
use crate::metrics::Metrics;
use crate::scripting::ScriptRunner;
use crate::tasks::backoff::{
    format_duration, parse_retry_after, plan_next_fetch, FetchOutcome, Schedule, SchedulerConfig,
};
use crate::tasks::cache::{
    corrected_max_age, extract_server_hints, parse_http_date, CacheControl, ServerHints,
};
use crate::tasks::command::TaskManagerCommand;
use crate::tasks::error::FetchError;
use crate::tasks::error_recording::{clear_feed_error, defer_feed, set_feed_error_with_schedule};
use crate::tasks::favicons::resolve_site_url;
use crate::tasks::processing::{
    enqueue_asset_caching, enqueue_favicon_caching, process_atom_feed, process_rss_feed,
};
use crate::tasks::scripting::{fire_fetch_error, fire_fetch_success};
use anyhow::Result;
use chrono::Utc;
use reqwest::header::HeaderMap;
use rusqlite::Connection;
use std::time::Instant;
use tracing::{debug, info, warn};

/// Snapshot of a `feeds` row loaded at the start of a refresh: the URL,
/// cache validators, body fingerprint, and scheduling state that drive the
/// fetch decision.
struct FeedFetchRow {
    title: String,
    url: String,
    header_etag: Option<String>,
    header_last_modified: Option<String>,
    header_expires: Option<i64>,
    header_immutable_until: Option<i64>,
    header_body_hash: Option<String>,
    last_full_refresh_at: Option<i64>,
    consecutive_failures: i64,
    next_fetch_at: Option<i64>,
    /// When the server's last `Retry-After` runs out, if it has not yet.
    retry_after_at: Option<i64>,
    min_fetch_interval: i64,
    auth: FeedAuth,
    /// Refresh hints from the last feed document that parsed, used when a
    /// response carries no body to read them from (a 304).
    feed_hints: FeedHints,
}

fn load_feed_fetch_row(conn: &Connection, feed_id: i64) -> Result<FeedFetchRow> {
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
            feed_skip_days,
            header_expires,
            title,
            retry_after_at
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
                title: row.get(18)?,
                url: row.get(0)?,
                header_etag: row.get(1)?,
                header_last_modified: row.get(2)?,
                header_immutable_until: row.get(3)?,
                header_body_hash: row.get(4)?,
                last_full_refresh_at: row.get(5)?,
                consecutive_failures: row.get(6)?,
                next_fetch_at: row.get(7)?,
                retry_after_at: row.get(19)?,
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
                header_expires: row.get(17)?,
            })
        },
    )?;
    Ok(row)
}

/// How a feed is named in the logs: its id, title, and URL, e.g.
/// `Feed 17 "watchTowr Labs" <https://labs.watchtowr.com/rss/>`.
fn feed_label(feed_id: i64, title: &str, url: &str) -> String {
    format!("Feed {} {:?} <{}>", feed_id, title, url)
}

/// Describe where a freshness hint came from, for the schedule explanation.
///
/// `http_hint` is the hint read from `headers`; the feed document's own
/// `<ttl>` / `sy:updatePeriod` is only used when there is none.
pub(super) fn hint_source(
    headers: &HeaderMap,
    http_hint: Option<u64>,
    feed_hints: &FeedHints,
) -> Option<String> {
    if http_hint.is_none() {
        return feed_hints
            .refresh_hint_secs()
            .map(|_| "the feed's <ttl>/sy:updatePeriod".to_string());
    }
    let joined = |name: &str| {
        let values: Vec<&str> = headers
            .get_all(name)
            .iter()
            .filter_map(|h| h.to_str().ok())
            .collect();
        (!values.is_empty()).then(|| values.join(", "))
    };
    let mut parts = Vec::new();
    match joined("cache-control") {
        Some(cc) if cc.to_ascii_lowercase().contains("max-age") => {
            parts.push(format!("Cache-Control {:?}", cc));
        }
        _ => {
            if let Some(expires) = joined("expires") {
                parts.push(format!("Expires {:?}", expires));
            }
        }
    }
    // An `Age` shortens the hint (RFC 9111 §4.2.3), so show it too.
    if let Some(age) = joined("age") {
        parts.push(format!("Age {:?}", age));
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// Refresh the feed corresponding to the provided `feed_id`.
///
/// Reads what the fetch needs out of the database, hands it to `fetcher`
/// — which may be another process — and records whatever comes back.
/// Every database write, script event, and metric happens here, in the
/// caller's process; the fetcher only ever sees a [`FetchSpec`].
///
/// A feed whose `next_fetch_at` is still in the future is skipped unless
/// the refresh is `manual`, i.e. one a user asked for; even then, it is
/// skipped while a server's `Retry-After` is in force. A manual refresh
/// still sends the stored validators, so an unchanged feed costs the
/// server a 304.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn refresh_feed(
    fetcher: &Fetcher,
    feed_id: i64,
    manual: bool,
    db: Db,
    settings: &Settings,
    script_runner: Option<&dyn ScriptRunner>,
    metrics: &Metrics,
    task_tx: &async_channel::Sender<TaskManagerCommand>,
) -> Result<()> {
    let fetch_start = Instant::now();
    let fetch_settings = &settings.feed_fetch;

    let row = db
        .read(move |conn| load_feed_fetch_row(conn, feed_id))
        .await??;

    let cfg = SchedulerConfig {
        min_cadence: fetch_settings.min_polling_cadence_seconds,
        max_backoff: fetch_settings.max_backoff_seconds,
        min_fetch_interval: row.min_fetch_interval.max(0) as u64,
        force_refresh_after: fetch_settings.force_refresh_after_seconds,
    };
    let rec = Recorder {
        db: &db,
        feed_id,
        label: feed_label(feed_id, &row.title, &row.url),
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
    // A refresh a user asked for ignores the schedule, but not a server's
    // `Retry-After`: that is the server asking us to stay away. The
    // scheduler only queues feeds that are due, so for it this mostly
    // catches a feed that another refresh rescheduled while this one sat in
    // the queue. No fetch happens, so it is counted as a skip and not under
    // `kiki_feed_fetch_total`.
    let not_before = if manual {
        // Never later than the scheduler would fetch, e.g. when the
        // Retry-After was beyond the backoff cap.
        match (row.retry_after_at, row.next_fetch_at) {
            (Some(retry_at), Some(next_ts)) => Some(retry_at.min(next_ts)),
            (retry_at, _) => retry_at,
        }
    } else {
        row.next_fetch_at
    };
    if let Some(next_ts) = not_before.filter(|&ts| now_ts < ts) {
        debug!(
            "{} not yet eligible; next fetch in {}",
            rec.label,
            format_duration(u64::try_from(next_ts - now_ts).unwrap_or(0))
        );
        metrics.record_feed_cache_hit("next_fetch_at");
        return Ok(());
    }

    // file:// feeds are read here, because the fetcher has no filesystem
    // access, and only the parse is handed off.
    let retrieved = if row.url.starts_with("file://") {
        let read = crate::db::blocking(|| retrieve_file_feed(&row.url, feed_id, &db, cfg));
        match read {
            Ok((content, schedule)) => Retrieved::File(
                fetcher
                    .parse(feed_id, content)
                    .await
                    .map(|parsed| (parsed, schedule)),
            ),
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
            proxy: settings.effective_proxy(),
        };
        Retrieved::Http(fetcher.fetch(spec).await)
    };

    crate::db::blocking(|| {
        store_refresh(
            &rec,
            &row,
            retrieved,
            force_conditionals_off,
            settings,
            task_tx,
        )
    })
}

/// What a refresh got back from the fetcher, before anything is stored.
enum Retrieved {
    /// A `file://` feed, read (and its row rescheduled) by
    /// [`retrieve_file_feed`], and then parsed.
    File(Result<(ParseOutcome, Schedule), FetcherError>),
    /// The fetcher's reply for an HTTP(S) feed.
    Http(Result<FetchReply, FetcherError>),
}

/// Record what a refresh retrieved: update the feed's row, store its
/// entries, and queue the follow-up work.
///
/// Blocks on the database and on scripts, so async callers run it under
/// [`crate::db::blocking`].
fn store_refresh(
    rec: &Recorder,
    row: &FeedFetchRow,
    retrieved: Retrieved,
    force_conditionals_off: bool,
    settings: &Settings,
    task_tx: &async_channel::Sender<TaskManagerCommand>,
) -> Result<()> {
    let (feed_id, metrics) = (rec.feed_id, rec.metrics);

    // Set when the server answers 304 Not Modified.
    let mut not_modified = false;
    let fetched = match retrieved {
        Retrieved::File(parsed) => parsed.map(Some),
        Retrieved::Http(Ok(reply)) => {
            not_modified = matches!(reply, FetchReply::NotModified { .. });
            match record_fetch_reply(rec, row, reply, force_conditionals_off) {
                Ok(parsed) => Ok(parsed),
                Err(e) => {
                    rec.outcome("other");
                    return Err(e);
                }
            }
        }
        Retrieved::Http(Err(e)) => Err(e),
    };

    let (parsed, schedule) = match fetched {
        Ok(Some(fetched)) => fetched,
        // The outcome (304, HTTP error, oversized body, ...) has been
        // recorded and there is nothing to store. An unchanged feed is
        // still a working one, so its favicon is looked for all the same:
        // otherwise a feed whose server keeps answering 304 would only get
        // one after its content changed.
        Ok(None) => {
            if not_modified {
                rec.db.read_blocking(|conn| {
                    queue_favicon_if_due(settings, conn, task_tx, metrics, feed_id)
                })?;
            }
            return Ok(());
        }
        // The fetcher process is gone, and the server stops when it
        // notices. That is no fault of the feed's, so nothing is recorded
        // against it: it is only put off, so that it is not queued again
        // and again in the meantime.
        Err(FetcherError::Gone) => {
            warn!("{}: not fetched: {}", rec.label, FetcherError::Gone);
            rec.db
                .write_blocking(|conn| defer_feed(conn, feed_id, rec.cfg.min_cadence))??;
            rec.outcome("fetcher_gone");
            return Ok(());
        }
        // The feed is what kills the fetcher's worker: it waits the full
        // backoff cap, like any other permanent error, rather than
        // crashing the worker again at the next opportunity.
        Err(FetcherError::Crashed(message)) => {
            let url = row.url.clone();
            rec.fail(
                FetchError::FetcherCrashed { url, message },
                "fetcher",
                "fetcher_crashed",
            );
            return Ok(());
        }
        // The fetcher itself failed, not the feed server. Recorded as a
        // transient error so the feed backs off rather than being retried
        // on the next tick.
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
    let entry_count = feed.entry_count();
    // The site link comes from the feed document, so it is only kept if it
    // is an http(s) URL; relative links are relative to the feed.
    let site_url = feed
        .site_url()
        .and_then(|u| resolve_site_url(u, &row.url))
        .map(String::from);
    let site_url = site_url.as_deref();
    let inserted = match feed {
        ParsedFeed::Atom { feed, entries, .. } => process_atom_feed(
            feed_id,
            site_url,
            *feed,
            entries,
            rec.db,
            rec.script_runner,
            metrics,
        )?,
        ParsedFeed::Rss { entries, .. } => process_rss_feed(
            feed_id,
            site_url,
            entries,
            rec.db,
            rec.script_runner,
            metrics,
        )?,
    };
    enqueue_asset_caching(task_tx, metrics, &inserted.cache_assets);
    rec.db
        .read_blocking(|conn| queue_favicon_if_due(settings, conn, task_tx, metrics, feed_id))?;
    rec.db
        .write_blocking(|conn| clear_feed_error(conn, feed_id))?;
    debug!(
        "{} refreshed with {} entries, {} new; next fetch {}",
        rec.label,
        entry_count,
        inserted.ids.len(),
        schedule
    );
    rec.outcome("success");
    Ok(())
}

/// Queue a look for feed `feed_id`'s favicon if the asset cache is enabled
/// and Kiki has not looked recently.
fn queue_favicon_if_due(
    settings: &Settings,
    conn: &rusqlite::Connection,
    task_tx: &async_channel::Sender<TaskManagerCommand>,
    metrics: &Metrics,
    feed_id: i64,
) {
    if settings.asset_cache.enabled && favicon_is_due(conn, feed_id) {
        enqueue_favicon_caching(task_tx, metrics, feed_id);
    }
}

/// Whether feed `feed_id`'s favicon should be looked for now. A database
/// error is logged and taken as no, since the next refresh will ask again.
fn favicon_is_due(conn: &rusqlite::Connection, feed_id: i64) -> bool {
    crate::tasks::favicons::is_due(conn, feed_id).unwrap_or_else(|e| {
        warn!("could not check the favicon of feed {}: {:?}", feed_id, e);
        false
    })
}

/// Everything the outcome of one refresh is recorded against.
struct Recorder<'a> {
    db: &'a Db,
    feed_id: i64,
    /// The feed as named in the logs; see [`feed_label`].
    label: String,
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
    ///
    fn fail_with(
        &self,
        err: FetchError,
        kind: &'static str,
        outcome: &'static str,
        status: Option<u16>,
        retry_after_ts: Option<i64>,
        stale_if_error: Option<u64>,
    ) {
        let schedule = match self.db.write_blocking(|conn| {
            set_feed_error_with_schedule(
                conn,
                self.feed_id,
                &err,
                retry_after_ts,
                stale_if_error,
                self.cfg.min_cadence,
                self.cfg.max_backoff,
                self.cfg.min_fetch_interval,
                self.metrics,
            )
        }) {
            Ok(schedule) => schedule,
            Err(e) => {
                warn!("{}: could not record the error: {}", self.label, e);
                None
            }
        };
        match schedule {
            Some(schedule) => warn!("{}: {}; next attempt {}", self.label, err, schedule),
            None => warn!("{}: {}", self.label, err),
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
/// Returns the parse result, and when the feed is next due, when the server
/// sent a `200 OK` whose body was read, and `None` for every outcome that
/// leaves nothing to store.
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
) -> Result<Option<(ParseOutcome, Schedule)>> {
    let (feed_id, cfg, metrics) = (rec.feed_id, rec.cfg, rec.metrics);
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
            let now_ts = Utc::now().timestamp();
            let headers = headers.to_header_map();
            let hints = extract_server_hints(&headers, now_ts);
            let cache = revalidated_cache_state(row, &headers, &hints, now_ts);
            let schedule = schedule_success(
                FetchOutcome::NotModified {
                    server_hint_secs: hints.hint_secs.or(row.feed_hints.refresh_hint_secs()),
                },
                &row.feed_hints,
                now_ts,
                cfg,
            )
            .with_hint_source(hint_source(&headers, hints.hint_secs, &row.feed_hints));
            let next_fetch_at = schedule.next_fetch_at;
            rec.db.write_blocking(|conn| {
                conn.execute(
                    "UPDATE feeds SET
                    header_etag = ?1,
                    header_last_modified = ?2,
                    header_expires = ?3,
                    header_immutable_until = ?4,
                    last_checked = ?5,
                    next_fetch_at = ?6,
                    consecutive_failures = 0,
                    retry_after_at = NULL
                 WHERE id = ?7",
                    rusqlite::params![
                        cache.etag,
                        cache.last_modified,
                        cache.expires,
                        cache.immutable_until,
                        now_ts,
                        next_fetch_at,
                        feed_id,
                    ],
                )
            })??;
            debug!("{} was not modified; next fetch {}", rec.label, schedule);
            metrics.record_feed_cache_hit("not_modified");
            metrics.record_feed_retry_scheduled(
                schedule.reason.metric_source(),
                (next_fetch_at - now_ts) as f64,
            );
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
            debug!("{}: gave up on the body after {} bytes", rec.label, seen);
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
    // If we followed a permanent redirect, the feed now lives at
    // `final_url`. If another feed has that URL already, the two are the
    // same feed, so this one is merged into it and there is nothing left
    // to store. Otherwise the stored URL is updated to match.
    if permanent_redirect && final_url != feed_url {
        let merged = rec.db.write_blocking(|conn| -> Result<_> {
            let merged = merge_feed_into(conn, feed_id, &final_url)?;
            if merged.is_none() {
                conn.execute(
                    "UPDATE feeds SET url = ?1 WHERE id = ?2",
                    (&final_url, feed_id),
                )?;
            }
            Ok(merged)
        })??;
        if let Some(merged) = merged {
            info!(
                "{} permanently redirected to {}, the URL of feed {}; merged it into that feed",
                rec.label, final_url, merged.into
            );
            if let Some(runner) = rec.script_runner {
                runner.dispatch_observe(
                    crate::scripting::Event::FeedRemoved,
                    crate::scripting::EventPayload::Feed {
                        id: feed_id,
                        url: merged.url,
                        title: merged.title,
                    },
                );
            }
            rec.outcome("merged");
            return Ok(None);
        }
        info!(
            "{} permanently redirected to {}; updated the stored URL",
            rec.label, final_url
        );
    }

    let now_ts = Utc::now().timestamp();
    let hints = extract_server_hints(&headers, now_ts);
    let CacheState {
        etag,
        last_modified,
        expires,
        immutable_until,
        no_store,
    } = cache_state(&headers, &hints, now_ts);

    // Refresh hints from the feed document. A body that did not parse
    // leaves the previously stored ones in force.
    let feed_hints = parsed
        .feed
        .as_ref()
        .map_or(row.feed_hints, |feed| *feed.hints());

    // HTTP freshness takes precedence (RFC 9111 is specific to this
    // representation); the feed's own <ttl> / sy:update* is the fallback.
    let schedule = schedule_success(
        FetchOutcome::Success {
            server_hint_secs: hints.hint_secs.or(feed_hints.refresh_hint_secs()),
        },
        &feed_hints,
        now_ts,
        cfg,
    )
    .with_hint_source(hint_source(&headers, hints.hint_secs, &feed_hints));
    let next_fetch_at = schedule.next_fetch_at;

    metrics.record_feed_response_bytes(body_len);

    // Don't persist a body-hash fingerprint for responses we've been told
    // not to store (RFC 9111 §5.2.2 no-store).
    let body_hash: Option<String> = if no_store { None } else { Some(body_hash) };

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
                "{}: server returned unchanged ETag/Last-Modified but body hash differs; validators appear untrustworthy",
                rec.label
            );
            metrics.record_feed_validator_lie();
        }
        metrics.record_feed_forced_refresh(if body_changed { "mismatch" } else { "match" });
    }

    rec.db.write_blocking(|conn| {
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
        )
    })??;

    metrics.record_feed_retry_scheduled(
        schedule.reason.metric_source(),
        (next_fetch_at - now_ts) as f64,
    );

    fire_fetch_success(rec.script_runner, feed_id, 200, final_url, Some(body_len));

    Ok(Some((parsed, schedule)))
}

/// The cache validators and freshness state stored on a feed row, derived
/// from one response's headers.
struct CacheState {
    etag: Option<String>,
    last_modified: Option<String>,
    /// Absolute expiry: `max-age` (age-corrected) or else `Expires`.
    expires: Option<i64>,
    /// End of an RFC 8246 `immutable` window.
    immutable_until: Option<i64>,
    /// `Cache-Control: no-store` was present.
    no_store: bool,
}

/// Derive the cache state to store from a response's headers.
///
/// `hints` must be [`extract_server_hints`] of the same headers at
/// `now_ts`. Applies the Cache-Control precedence rules (RFC 9111 §5.2):
/// `no-store` clears everything, `no-cache` clears the expiry but keeps the
/// validators for conditional requests, and `max-age` overrides `Expires`
/// after the RFC 9111 §4.2.3 age correction.
fn cache_state(headers: &HeaderMap, hints: &ServerHints, now_ts: i64) -> CacheState {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|h| h.to_str().ok())
            .map(str::to_string)
    };
    let mut etag = header("etag");
    let mut last_modified = header("last-modified");

    // Accepts all three HTTP-date formats (RFC 9110 §5.6.7).
    let mut expires: Option<i64> = header("expires")
        .as_deref()
        .and_then(parse_http_date)
        .map(|dt| dt.timestamp());

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
        let corrected = corrected_max_age(headers, max_age, now_ts);
        expires = Some(now_ts + corrected as i64);
    }

    // RFC 8246: while the response is fresh, skip conditional revalidation
    // entirely. Only meaningful when paired with a positive max-age.
    let immutable_until = hints
        .hint_secs
        .filter(|&s| hints.immutable && s > 0)
        .map(|s| now_ts.saturating_add(s as i64));

    CacheState {
        etag,
        last_modified,
        expires,
        immutable_until,
        no_store: cc.no_store,
    }
}

/// The cache state to store after a `304 Not Modified`.
///
/// A 304 freshens the stored response: header fields it carries replace
/// the stored ones, and fields it omits keep their stored values
/// (RFC 9111 §4.3.4). So a server that rotates its `ETag` or
/// `Last-Modified` on a 304 has the new validator sent next time, and one
/// that re-sends `Cache-Control: immutable` opens a new immutable window.
/// `no-store` on the 304 still clears everything.
fn revalidated_cache_state(
    row: &FeedFetchRow,
    headers: &HeaderMap,
    hints: &ServerHints,
    now_ts: i64,
) -> CacheState {
    let fresh = cache_state(headers, hints, now_ts);
    if fresh.no_store {
        return fresh;
    }
    let carries_freshness =
        headers.contains_key("cache-control") || headers.contains_key("expires");
    CacheState {
        etag: fresh.etag.or_else(|| row.header_etag.clone()),
        last_modified: fresh
            .last_modified
            .or_else(|| row.header_last_modified.clone()),
        expires: if carries_freshness {
            fresh.expires
        } else {
            row.header_expires
        },
        immutable_until: if carries_freshness {
            fresh.immutable_until
        } else {
            row.header_immutable_until
        },
        no_store: false,
    }
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
) -> Schedule {
    plan_next_fetch(
        outcome,
        now_ts,
        cfg.min_cadence,
        cfg.max_backoff,
        cfg.min_fetch_interval,
    )
    .defer_past_skipped(feed_hints.skip_hours, feed_hints.skip_days)
}

fn retrieve_file_feed(
    feed_url: &str,
    feed_id: i64,
    db: &Db,
    cfg: SchedulerConfig,
) -> Result<(Vec<u8>, Schedule)> {
    // Extract the file path from the URL
    let file_path = feed_url.strip_prefix("file://").unwrap_or(feed_url);

    // Read the file content
    let content = std::fs::read(file_path)
        .map_err(|e| anyhow::anyhow!("Failed to read file {}: {}", file_path, e))?;

    // File-backed feeds have no cache headers; schedule using the per-feed
    // interval (clamped to the global floor and ceiling).
    let now_ts = Utc::now().timestamp();
    let schedule = plan_next_fetch(
        FetchOutcome::Success {
            server_hint_secs: None,
        },
        now_ts,
        cfg.min_cadence,
        cfg.max_backoff,
        cfg.min_fetch_interval,
    );
    let next_fetch_at = schedule.next_fetch_at;
    db.write_blocking(|conn| {
        conn.execute(
            "UPDATE feeds SET
            last_checked = ?1,
            next_fetch_at = ?2,
            consecutive_failures = 0,
            retry_after_at = NULL
         WHERE id = ?3",
            (now_ts, next_fetch_at, feed_id),
        )
    })??;

    Ok((content, schedule))
}
