use chrono::{DateTime, Datelike, SecondsFormat, Timelike, Utc};
use reqwest::Url;
use std::fmt;

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

/// Compute when a feed is next eligible to be fetched, and why.
///
/// Rules (all results clamped to `[now + min_cadence, now + max_backoff]`):
/// - `Success` / `NotModified`: use `min(server_hint, min_fetch_interval)`
///   when a hint is present; otherwise use `min_fetch_interval`. The per-feed
///   `min_fetch_interval` is a ceiling, so we never wait longer than it.
///   A wait from a hint shorter than the interval may then be lengthened by
///   a `fetch.schedule` plugin; see [`Schedule::stretched_by_plugin`].
/// - `TransientErr` with `Retry-After`: honor the server's deadline.
/// - `TransientErr` without `Retry-After`: exponential backoff
///   `min_cadence * 2^(consecutive_failures - 1)`.
/// - `PermanentErr`: wait the full `max_backoff`.
///
/// The returned [`Schedule`] records which rule decided the time, and any
/// clamping, so it can be logged.
///
/// # Examples
///
/// ```ignore
/// let schedule = plan_next_fetch(
///     FetchOutcome::NotModified { server_hint_secs: Some(0) },
///     now_ts,
///     60,
///     86_400,
///     10_800,
/// );
/// assert_eq!(schedule.next_fetch_at, now_ts + 60);
/// assert_eq!(schedule.clamp, Some(ScheduleClamp::MinCadence { secs: 60 }));
/// ```
pub(super) fn plan_next_fetch(
    outcome: FetchOutcome,
    now_ts: i64,
    min_cadence: u64,
    max_backoff: u64,
    min_fetch_interval: u64,
) -> Schedule {
    let min_cadence = min_cadence.max(1);
    let max_backoff = max_backoff.max(min_cadence);

    let (raw_next_ts, reason): (i64, ScheduleReason) = match outcome {
        FetchOutcome::Success { server_hint_secs }
        | FetchOutcome::NotModified { server_hint_secs } => {
            let reason = match server_hint_secs {
                Some(hint) if hint < min_fetch_interval => ScheduleReason::FreshnessHint {
                    secs: hint,
                    interval_secs: min_fetch_interval,
                },
                hint_secs => ScheduleReason::FeedInterval {
                    secs: min_fetch_interval,
                    hint_secs,
                },
            };
            let interval = match server_hint_secs {
                Some(hint) => hint.min(min_fetch_interval),
                None => min_fetch_interval,
            };
            (now_ts.saturating_add(interval as i64), reason)
        }
        FetchOutcome::TransientErr {
            retry_after_ts: Some(ts),
            stale_if_error_secs,
            ..
        } => {
            // Don't wait past the stale-if-error window (RFC 5861 §4).
            let stale_until = stale_if_error_secs.map(|s| now_ts.saturating_add(s as i64));
            match stale_until {
                Some(until) if until < ts => (
                    until,
                    ScheduleReason::StaleIfError {
                        secs: stale_if_error_secs.unwrap_or(0),
                    },
                ),
                _ => (ts, ScheduleReason::RetryAfter),
            }
        }
        FetchOutcome::TransientErr {
            retry_after_ts: None,
            consecutive_failures,
            stale_if_error_secs,
        } => {
            let shift = consecutive_failures.saturating_sub(1).min(63);
            let multiplier = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
            let backoff = min_cadence.saturating_mul(multiplier);
            match stale_if_error_secs {
                Some(s) if s < backoff => (
                    now_ts.saturating_add(s as i64),
                    ScheduleReason::StaleIfError { secs: s },
                ),
                _ => (
                    now_ts.saturating_add(backoff as i64),
                    ScheduleReason::Backoff {
                        consecutive_failures,
                    },
                ),
            }
        }
        FetchOutcome::PermanentErr => (
            now_ts.saturating_add(max_backoff as i64),
            ScheduleReason::PermanentError,
        ),
    };

    let floor = now_ts.saturating_add(min_cadence as i64);
    let ceiling = now_ts.saturating_add(max_backoff as i64);
    let next_fetch_at = raw_next_ts.max(floor).min(ceiling);
    // A permanent error waits the full backoff cap by design; saying it was
    // "capped" would only be noise.
    let clamp = if raw_next_ts < floor {
        Some(ScheduleClamp::MinCadence { secs: min_cadence })
    } else if raw_next_ts > ceiling && !matches!(reason, ScheduleReason::PermanentError) {
        Some(ScheduleClamp::MaxBackoff { secs: max_backoff })
    } else {
        None
    };

    Schedule {
        now_ts,
        next_fetch_at,
        reason,
        clamp,
        deferred_for_skip: false,
        hint_source: None,
    }
}

/// [`plan_next_fetch`]'s timestamp alone.
#[cfg(test)]
pub(super) fn compute_next_fetch_at(
    outcome: FetchOutcome,
    now_ts: i64,
    min_cadence: u64,
    max_backoff: u64,
    min_fetch_interval: u64,
) -> i64 {
    plan_next_fetch(
        outcome,
        now_ts,
        min_cadence,
        max_backoff,
        min_fetch_interval,
    )
    .next_fetch_at
}

/// When a feed will next be fetched, and what decided it.
///
/// Built by [`plan_next_fetch`]. Its [`Display`](fmt::Display) form is a
/// short human-readable explanation for the logs, e.g.
/// `in 1m at 2026-09-28T18:41:42Z (freshness hint of 0s, under the feed's
/// 3h interval; hint from Cache-Control "public, max-age=0", raised to the
/// 1m minimum polling cadence)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Schedule {
    /// When the schedule was computed, as a Unix timestamp.
    pub(crate) now_ts: i64,
    /// When the feed is next eligible to be fetched, as a Unix timestamp.
    pub(crate) next_fetch_at: i64,
    /// The rule that picked the wait.
    pub(crate) reason: ScheduleReason,
    /// Set when the wait was moved into the global cadence bounds.
    pub(crate) clamp: Option<ScheduleClamp>,
    /// Set when the time was pushed out of the feed's `<skipHours>` or
    /// `<skipDays>`.
    pub(crate) deferred_for_skip: bool,
    /// Where the freshness hint came from, e.g. `Cache-Control "max-age=0"`,
    /// when the caller knows. Only shown when there was a hint.
    pub(crate) hint_source: Option<String>,
}

/// The rule that decided how long a feed waits before its next fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ScheduleReason {
    /// A freshness hint of `secs` shorter than the feed's interval of
    /// `interval_secs`. The hint came from the HTTP cache headers or the
    /// feed's own `<ttl>` / `sy:updatePeriod`.
    FreshnessHint { secs: u64, interval_secs: u64 },
    /// A freshness hint of `hint_secs`, whose wait the `fetch.schedule`
    /// handler of `plugin` lengthened to `secs`.
    Plugin {
        plugin: String,
        hint_secs: u64,
        secs: u64,
    },
    /// The feed's own fetch interval; either there was no freshness hint
    /// or the hint (`hint_secs`) was longer than the interval.
    FeedInterval { secs: u64, hint_secs: Option<u64> },
    /// The server's `Retry-After`.
    RetryAfter,
    /// Retry before the server's `stale-if-error` window of `secs` ends.
    StaleIfError { secs: u64 },
    /// Exponential backoff after this many failures in a row.
    Backoff { consecutive_failures: u32 },
    /// A permanent error; the feed waits the full backoff cap.
    PermanentError,
}

impl ScheduleReason {
    /// The `source` label this reason is counted under in
    /// `kiki_feed_retry_scheduled_seconds`.
    pub(crate) fn metric_source(&self) -> &'static str {
        match self {
            ScheduleReason::FreshnessHint { .. } => "cache_hint",
            ScheduleReason::Plugin { .. } => "plugin",
            ScheduleReason::FeedInterval { .. } => "interval",
            ScheduleReason::RetryAfter => "retry_after",
            ScheduleReason::StaleIfError { .. } => "stale_if_error",
            ScheduleReason::Backoff { .. } => "backoff",
            ScheduleReason::PermanentError => "permanent",
        }
    }
}

/// How a computed wait was moved into the global cadence bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScheduleClamp {
    /// Raised to `feed_fetch.min_polling_cadence_seconds`.
    MinCadence { secs: u64 },
    /// Lowered to `feed_fetch.max_backoff_seconds`.
    MaxBackoff { secs: u64 },
}

impl Schedule {
    /// Move the scheduled time out of the feed's `<skipHours>` /
    /// `<skipDays>`, noting it if that changed anything.
    ///
    /// See [`defer_past_skipped`].
    pub(super) fn defer_past_skipped(mut self, skip_hours: u32, skip_days: u8) -> Schedule {
        let deferred = defer_past_skipped(self.next_fetch_at, skip_hours, skip_days);
        if deferred != self.next_fetch_at {
            self.next_fetch_at = deferred;
            self.deferred_for_skip = true;
        }
        self
    }

    /// Record where the freshness hint came from, for the explanation.
    pub(super) fn with_hint_source(mut self, source: Option<String>) -> Schedule {
        self.hint_source = source;
        self
    }

    /// The room a `fetch.schedule` plugin has to lengthen this schedule's
    /// wait: the wait itself, and the longest it may be lengthened to, which
    /// is the feed's interval (never past `max_backoff`). `None` unless the
    /// wait came from a freshness hint shorter than the feed's interval, and
    /// is still shorter than that.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // A 0s hint on a feed with a 1h interval, raised to a 1m cadence.
    /// let schedule = plan_next_fetch(
    ///     FetchOutcome::NotModified { server_hint_secs: Some(0) },
    ///     now_ts, 60, 86_400, 3_600,
    /// );
    /// assert_eq!(schedule.plugin_room(86_400), Some((60, 3_600)));
    /// ```
    pub(super) fn plugin_room(&self, max_backoff: u64) -> Option<(u64, u64)> {
        let ScheduleReason::FreshnessHint { interval_secs, .. } = self.reason else {
            return None;
        };
        let wait = self.wait_secs();
        let ceiling = interval_secs.min(max_backoff);
        (wait < ceiling).then_some((wait, ceiling))
    }

    /// This schedule with its wait lengthened to `secs` by `plugin`'s
    /// `fetch.schedule` handler, within the bounds of
    /// [`Self::plugin_room`]. Returned as it is when there is no room, or
    /// `secs` would not lengthen the wait.
    pub(super) fn stretched_by_plugin(
        mut self,
        plugin: String,
        secs: u64,
        max_backoff: u64,
    ) -> Schedule {
        let Some((wait, ceiling)) = self.plugin_room(max_backoff) else {
            return self;
        };
        let ScheduleReason::FreshnessHint {
            secs: hint_secs, ..
        } = self.reason
        else {
            return self;
        };
        let secs = secs.min(ceiling);
        if secs <= wait {
            return self;
        }
        self.next_fetch_at = self
            .now_ts
            .saturating_add(i64::try_from(secs).unwrap_or(i64::MAX));
        self.reason = ScheduleReason::Plugin {
            plugin,
            hint_secs,
            secs,
        };
        // Lengthened past the floor it may have been raised to.
        self.clamp = None;
        self
    }

    /// Seconds from when the schedule was computed until the next fetch.
    pub(crate) fn wait_secs(&self) -> u64 {
        u64::try_from(self.next_fetch_at.saturating_sub(self.now_ts)).unwrap_or(0)
    }
}

impl fmt::Display for Schedule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let at = DateTime::<Utc>::from_timestamp(self.next_fetch_at, 0)
            .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Secs, true))
            .unwrap_or_else(|| self.next_fetch_at.to_string());
        write!(
            f,
            "in {} at {} ({}",
            format_duration(self.wait_secs()),
            at,
            self.reason
        )?;
        let had_hint = matches!(
            self.reason,
            ScheduleReason::FreshnessHint { .. }
                | ScheduleReason::Plugin { .. }
                | ScheduleReason::FeedInterval {
                    hint_secs: Some(_),
                    ..
                }
        );
        if let (true, Some(source)) = (had_hint, &self.hint_source) {
            write!(f, "; hint from {}", source)?;
        }
        match self.clamp {
            Some(ScheduleClamp::MinCadence { secs }) => write!(
                f,
                ", raised to the {} minimum polling cadence",
                format_duration(secs)
            )?,
            Some(ScheduleClamp::MaxBackoff { secs }) => write!(
                f,
                ", capped at the {} maximum backoff",
                format_duration(secs)
            )?,
            None => {}
        }
        if self.deferred_for_skip {
            write!(f, ", deferred past the feed's skipHours/skipDays")?;
        }
        write!(f, ")")
    }
}

impl fmt::Display for ScheduleReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ScheduleReason::FreshnessHint {
                secs,
                interval_secs,
            } => write!(
                f,
                "freshness hint of {}, under the feed's {} interval",
                format_duration(secs),
                format_duration(interval_secs)
            ),
            ScheduleReason::Plugin {
                ref plugin,
                hint_secs,
                secs,
            } => write!(
                f,
                "freshness hint of {}, stretched to {} by the {} plugin",
                format_duration(hint_secs),
                format_duration(secs),
                plugin
            ),
            ScheduleReason::FeedInterval {
                secs,
                hint_secs: None,
            } => write!(
                f,
                "feed interval of {}, no freshness hint",
                format_duration(secs)
            ),
            ScheduleReason::FeedInterval {
                secs,
                hint_secs: Some(hint),
            } => write!(
                f,
                "feed interval of {}, freshness hint of {}",
                format_duration(secs),
                format_duration(hint)
            ),
            ScheduleReason::RetryAfter => write!(f, "server's Retry-After"),
            ScheduleReason::StaleIfError { secs } => write!(
                f,
                "retrying within the server's {} stale-if-error window",
                format_duration(secs)
            ),
            ScheduleReason::Backoff {
                consecutive_failures,
            } => write!(
                f,
                "backoff after {} consecutive failure{}",
                consecutive_failures,
                if consecutive_failures == 1 { "" } else { "s" }
            ),
            ScheduleReason::PermanentError => write!(f, "permanent error"),
        }
    }
}

/// Format a number of seconds compactly for the logs, using at most the
/// two largest units: `45s`, `1m`, `3h`, `2h 5m`, `1d 6h`.
pub(crate) fn format_duration(secs: u64) -> String {
    const UNITS: [(u64, &str); 4] = [(86_400, "d"), (3_600, "h"), (60, "m"), (1, "s")];
    if secs == 0 {
        return "0s".to_string();
    }
    let mut rest = secs;
    let mut parts = Vec::with_capacity(2);
    for (size, unit) in UNITS {
        if parts.len() == 2 {
            break;
        }
        let n = rest / size;
        if n > 0 {
            parts.push(format!("{}{}", n, unit));
            rest -= n * size;
        } else if !parts.is_empty() {
            // Don't skip a unit between two: `1d 30s` reads as `1d`.
            break;
        }
    }
    parts.join(" ")
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
