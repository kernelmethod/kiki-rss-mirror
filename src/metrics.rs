//! Prometheus metrics surface.
//!
//! Each server instance owns a [`Metrics`] value that wraps a private PrometheusRecorder:
//! instrumentation calls from route handlers, task workers, and the sampler loop all go through
//! methods on this struct rather than the global `metrics` facade. That gives us per-server
//! isolation (useful for tests, which each get a fresh recorder) and lets `/metrics` render just
//! the state for its own server.
//!
//! When the `metrics` feature is disabled, [`Metrics`] becomes a zero-sized type whose methods are
//! no-ops, so call sites stay free of `cfg` attributes.

#[cfg(feature = "metrics")]
pub use imp::{handle_metrics, track_http, Metrics};

#[cfg(not(feature = "metrics"))]
pub use stub::Metrics;

#[cfg(feature = "metrics")]
mod imp {
    use crate::server::AppState;
    use axum::{
        body::Body,
        extract::{MatchedPath, Request, State},
        http::HeaderValue,
        middleware::Next,
        response::{IntoResponse, Response},
    };
    use metrics::{Key, KeyName, Label, Metadata, Recorder, SharedString};
    use metrics_exporter_prometheus::{
        Matcher, PrometheusBuilder, PrometheusHandle, PrometheusRecorder,
    };
    use std::sync::Arc;
    use std::time::Instant;

    const HTTP_DURATION_BUCKETS: &[f64] = &[
        0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
    ];

    const FETCH_DURATION_BUCKETS: &[f64] =
        &[0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0];

    const RESPONSE_BYTE_BUCKETS: &[f64] = &[
        1024.0,
        4096.0,
        16_384.0,
        65_536.0,
        262_144.0,
        1_048_576.0,
        4_194_304.0,
        16_777_216.0,
    ];

    const REDIRECT_BUCKETS: &[f64] = &[0.0, 1.0, 2.0, 3.0, 5.0, 10.0];

    // Seconds-until-next-fetch buckets: covers the 60s floor through the
    // 24h default backoff cap.
    const SCHEDULE_BUCKETS: &[f64] = &[
        60.0, 300.0, 900.0, 3_600.0, 10_800.0, 21_600.0, 43_200.0, 86_400.0,
    ];

    // Consecutive-failure streak length buckets.
    const FAILURE_STREAK_BUCKETS: &[f64] = &[1.0, 2.0, 3.0, 5.0, 10.0, 20.0];

    static METADATA: Metadata<'static> =
        Metadata::new("kiki_rss::metrics", metrics::Level::INFO, None);

    /// Per-server Prometheus metrics recorder.
    ///
    /// Wraps a private [`PrometheusRecorder`] and its [`PrometheusHandle`].
    /// Clone-wrapped in an [`Arc`] and stored on `AppState` so every part of
    /// the server can emit samples into the same registry.
    pub struct Metrics {
        recorder: PrometheusRecorder,
        handle: PrometheusHandle,
    }

    impl Metrics {
        /// Build a fresh recorder with kiki's histogram buckets and register
        /// every metric family the crate emits so scrape output includes
        /// `# HELP` / `# TYPE` lines even before any sample has been recorded.
        pub fn new() -> anyhow::Result<Self> {
            let recorder = PrometheusBuilder::new()
                .set_buckets_for_metric(
                    Matcher::Full("kiki_http_request_duration_seconds".to_string()),
                    HTTP_DURATION_BUCKETS,
                )?
                .set_buckets_for_metric(
                    Matcher::Full("kiki_feed_fetch_duration_seconds".to_string()),
                    FETCH_DURATION_BUCKETS,
                )?
                .set_buckets_for_metric(
                    Matcher::Full("kiki_feed_parse_duration_seconds".to_string()),
                    FETCH_DURATION_BUCKETS,
                )?
                .set_buckets_for_metric(
                    Matcher::Full("kiki_feed_response_bytes".to_string()),
                    RESPONSE_BYTE_BUCKETS,
                )?
                .set_buckets_for_metric(
                    Matcher::Full("kiki_feed_redirects".to_string()),
                    REDIRECT_BUCKETS,
                )?
                .set_buckets_for_metric(
                    Matcher::Full("kiki_feed_retry_scheduled_seconds".to_string()),
                    SCHEDULE_BUCKETS,
                )?
                .set_buckets_for_metric(
                    Matcher::Full("kiki_feed_consecutive_failures".to_string()),
                    FAILURE_STREAK_BUCKETS,
                )?
                .set_buckets_for_metric(
                    Matcher::Full("kiki_task_duration_seconds".to_string()),
                    FETCH_DURATION_BUCKETS,
                )?
                .set_buckets_for_metric(
                    Matcher::Full("kiki_retention_cleanup_duration_seconds".to_string()),
                    FETCH_DURATION_BUCKETS,
                )?
                .set_buckets_for_metric(
                    Matcher::Full("kiki_db_pool_acquire_duration_seconds".to_string()),
                    HTTP_DURATION_BUCKETS,
                )?
                .set_buckets_for_metric(
                    Matcher::Full("kiki_plugin_execution_duration_seconds".to_string()),
                    HTTP_DURATION_BUCKETS,
                )?
                .build_recorder();
            let handle = recorder.handle();
            let m = Self { recorder, handle };
            m.describe_all();
            m.set_build_info();
            Ok(m)
        }

        /// Render the current state in Prometheus text exposition format.
        pub fn render(&self) -> String {
            self.handle.render()
        }

        fn describe_all(&self) {
            let r = &self.recorder;
            r.describe_counter(
                KeyName::from_const_str("kiki_http_requests_total"),
                None,
                SharedString::const_str(
                    "Total HTTP requests handled by the kiki server, labeled by method, matched route, and status code.",
                ),
            );
            r.describe_histogram(
                KeyName::from_const_str("kiki_http_request_duration_seconds"),
                None,
                SharedString::const_str("Duration of HTTP requests handled by the kiki server."),
            );
            r.describe_gauge(
                KeyName::from_const_str("kiki_http_requests_in_flight"),
                None,
                SharedString::const_str(
                    "HTTP requests currently being processed by the kiki server.",
                ),
            );

            r.describe_counter(
                KeyName::from_const_str("kiki_feed_fetch_total"),
                None,
                SharedString::const_str("Total outbound feed fetch attempts, labeled by outcome."),
            );
            r.describe_histogram(
                KeyName::from_const_str("kiki_feed_fetch_duration_seconds"),
                None,
                SharedString::const_str(
                    "Duration of outbound feed fetch attempts, including redirects and parsing.",
                ),
            );
            r.describe_histogram(
                KeyName::from_const_str("kiki_feed_response_bytes"),
                None,
                SharedString::const_str(
                    "Size in bytes of feed responses received from upstream servers.",
                ),
            );
            r.describe_histogram(
                KeyName::from_const_str("kiki_feed_redirects"),
                None,
                SharedString::const_str("Number of HTTP redirects followed per feed fetch."),
            );
            r.describe_histogram(
                KeyName::from_const_str("kiki_feed_parse_duration_seconds"),
                None,
                SharedString::const_str("Duration of feed parsing, labeled by format."),
            );
            r.describe_counter(
                KeyName::from_const_str("kiki_feed_entries_parsed_total"),
                None,
                SharedString::const_str("Total feed entries parsed, labeled by format."),
            );
            r.describe_counter(
                KeyName::from_const_str("kiki_feed_entries_upserted_total"),
                None,
                SharedString::const_str(
                    "Total feed entries inserted or replaced in the database, labeled by format.",
                ),
            );
            r.describe_counter(
                KeyName::from_const_str("kiki_feed_cache_hits_total"),
                None,
                SharedString::const_str(
                    "Total times a feed fetch was skipped due to a cache directive, labeled by reason.",
                ),
            );
            r.describe_counter(
                KeyName::from_const_str("kiki_feed_forced_refresh_total"),
                None,
                SharedString::const_str(
                    "Total forced (non-conditional) feed fetches, labeled by whether the body matched the stored hash (`match`) or not (`mismatch`).",
                ),
            );
            r.describe_counter(
                KeyName::from_const_str("kiki_feed_validator_lie_total"),
                None,
                SharedString::const_str(
                    "Total detected instances of a server returning unchanged ETag/Last-Modified validators alongside a changed response body.",
                ),
            );
            r.describe_histogram(
                KeyName::from_const_str("kiki_feed_retry_scheduled_seconds"),
                None,
                SharedString::const_str(
                    "Seconds until the next scheduled fetch attempt, labeled by the hint source (cache_hint, retry_after, backoff, permanent).",
                ),
            );
            r.describe_histogram(
                KeyName::from_const_str("kiki_feed_consecutive_failures"),
                None,
                SharedString::const_str(
                    "Length of the consecutive-failure streak at the moment a transient error was recorded.",
                ),
            );

            r.describe_counter(
                KeyName::from_const_str("kiki_tasks_enqueued_total"),
                None,
                SharedString::const_str("Total task-manager commands enqueued, labeled by type."),
            );
            r.describe_counter(
                KeyName::from_const_str("kiki_tasks_processed_total"),
                None,
                SharedString::const_str(
                    "Total task-manager commands processed, labeled by type and outcome.",
                ),
            );
            r.describe_histogram(
                KeyName::from_const_str("kiki_task_duration_seconds"),
                None,
                SharedString::const_str("Duration of task-manager commands, labeled by type."),
            );
            r.describe_gauge(
                KeyName::from_const_str("kiki_task_queue_depth"),
                None,
                SharedString::const_str(
                    "Current number of pending commands in the task-manager queue.",
                ),
            );
            r.describe_gauge(
                KeyName::from_const_str("kiki_workers_total"),
                None,
                SharedString::const_str("Total number of task-manager worker tasks spawned."),
            );
            r.describe_gauge(
                KeyName::from_const_str("kiki_workers_busy"),
                None,
                SharedString::const_str("Task-manager workers currently processing a command."),
            );
            r.describe_gauge(
                KeyName::from_const_str("kiki_feeds_refresh_in_progress"),
                None,
                SharedString::const_str("Feeds currently being refreshed by a worker."),
            );

            r.describe_gauge(
                KeyName::from_const_str("kiki_db_pool_connections"),
                None,
                SharedString::const_str("Total connections in the database connection pool."),
            );
            r.describe_gauge(
                KeyName::from_const_str("kiki_db_pool_connections_idle"),
                None,
                SharedString::const_str("Idle connections in the database connection pool."),
            );
            r.describe_gauge(
                KeyName::from_const_str("kiki_db_pool_connections_in_use"),
                None,
                SharedString::const_str("In-use connections in the database connection pool."),
            );
            r.describe_histogram(
                KeyName::from_const_str("kiki_db_pool_acquire_duration_seconds"),
                None,
                SharedString::const_str(
                    "Duration of database connection acquisition from the pool.",
                ),
            );
            r.describe_counter(
                KeyName::from_const_str("kiki_db_pool_acquire_errors_total"),
                None,
                SharedString::const_str(
                    "Total errors when acquiring a database connection from the pool.",
                ),
            );

            r.describe_histogram(
                KeyName::from_const_str("kiki_retention_cleanup_duration_seconds"),
                None,
                SharedString::const_str(
                    "Duration of retention cleanup operations, labeled by scope.",
                ),
            );
            r.describe_counter(
                KeyName::from_const_str("kiki_entries_deleted_total"),
                None,
                SharedString::const_str(
                    "Total entries deleted by retention cleanup, labeled by scope.",
                ),
            );

            r.describe_gauge(
                KeyName::from_const_str("kiki_feeds_total"),
                None,
                SharedString::const_str("Total feeds currently configured."),
            );
            r.describe_gauge(
                KeyName::from_const_str("kiki_entries_total"),
                None,
                SharedString::const_str("Total entries currently stored in the database."),
            );
            r.describe_gauge(
                KeyName::from_const_str("kiki_feeds_with_fetch_error"),
                None,
                SharedString::const_str("Feeds whose most recent fetch attempt recorded an error."),
            );

            r.describe_gauge(
                KeyName::from_const_str("kiki_plugins_loaded"),
                None,
                SharedString::const_str("Number of plugins currently loaded."),
            );
            r.describe_counter(
                KeyName::from_const_str("kiki_plugin_load_errors_total"),
                None,
                SharedString::const_str(
                    "Total failed attempts to load plugins, e.g. because a plugin failed to compile.",
                ),
            );
            r.describe_counter(
                KeyName::from_const_str("kiki_plugin_executions_total"),
                None,
                SharedString::const_str(
                    "Total entries run through the plugins' `entry.ingest` handlers, labeled by outcome (`ok`, `filtered`, `error`).",
                ),
            );
            r.describe_histogram(
                KeyName::from_const_str("kiki_plugin_execution_duration_seconds"),
                None,
                SharedString::const_str(
                    "Duration of running an entry through the plugins' `entry.ingest` handlers.",
                ),
            );

            r.describe_gauge(
                KeyName::from_const_str("kiki_build_info"),
                None,
                SharedString::const_str("Build information for the running kiki binary; always 1."),
            );
        }

        fn set_build_info(&self) {
            let key = Key::from_parts(
                "kiki_build_info",
                vec![Label::new("version", env!("CARGO_PKG_VERSION"))],
            );
            self.recorder.register_gauge(&key, &METADATA).set(1.0);
        }

        // ----- HTTP -----

        fn inc_http_requests(&self, method: &str, route: &str, status: &str) {
            let key = Key::from_parts(
                "kiki_http_requests_total",
                vec![
                    Label::new("method", method.to_string()),
                    Label::new("route", route.to_string()),
                    Label::new("status", status.to_string()),
                ],
            );
            self.recorder.register_counter(&key, &METADATA).increment(1);
        }

        fn record_http_duration(&self, method: &str, route: &str, seconds: f64) {
            let key = Key::from_parts(
                "kiki_http_request_duration_seconds",
                vec![
                    Label::new("method", method.to_string()),
                    Label::new("route", route.to_string()),
                ],
            );
            self.recorder
                .register_histogram(&key, &METADATA)
                .record(seconds);
        }

        fn http_in_flight(&self, method: &str, route: &str) -> metrics::Gauge {
            let key = Key::from_parts(
                "kiki_http_requests_in_flight",
                vec![
                    Label::new("method", method.to_string()),
                    Label::new("route", route.to_string()),
                ],
            );
            self.recorder.register_gauge(&key, &METADATA)
        }

        // ----- Feed fetching -----

        pub fn record_feed_fetch(&self, outcome: &'static str, duration_seconds: f64) {
            let key = Key::from_parts(
                "kiki_feed_fetch_total",
                vec![Label::new("outcome", outcome)],
            );
            self.recorder.register_counter(&key, &METADATA).increment(1);

            let key = Key::from_name("kiki_feed_fetch_duration_seconds");
            self.recorder
                .register_histogram(&key, &METADATA)
                .record(duration_seconds);
        }

        pub fn record_feed_response_bytes(&self, bytes: u64) {
            let key = Key::from_name("kiki_feed_response_bytes");
            self.recorder
                .register_histogram(&key, &METADATA)
                .record(bytes as f64);
        }

        pub fn record_feed_redirects(&self, hops: u64) {
            let key = Key::from_name("kiki_feed_redirects");
            self.recorder
                .register_histogram(&key, &METADATA)
                .record(hops as f64);
        }

        pub fn record_feed_parse(&self, format: &'static str, duration_seconds: f64, entries: u64) {
            let key = Key::from_parts(
                "kiki_feed_parse_duration_seconds",
                vec![Label::new("format", format)],
            );
            self.recorder
                .register_histogram(&key, &METADATA)
                .record(duration_seconds);

            let key = Key::from_parts(
                "kiki_feed_entries_parsed_total",
                vec![Label::new("format", format)],
            );
            self.recorder
                .register_counter(&key, &METADATA)
                .increment(entries);
        }

        pub fn record_feed_entry_upserted(&self, format: &'static str) {
            let key = Key::from_parts(
                "kiki_feed_entries_upserted_total",
                vec![Label::new("format", format)],
            );
            self.recorder.register_counter(&key, &METADATA).increment(1);
        }

        pub fn record_feed_cache_hit(&self, reason: &'static str) {
            let key = Key::from_parts(
                "kiki_feed_cache_hits_total",
                vec![Label::new("reason", reason)],
            );
            self.recorder.register_counter(&key, &METADATA).increment(1);
        }

        /// Record a forced (non-conditional) feed refresh, labeled by
        /// whether the freshly-fetched body matched the hash we stored on
        /// the previous full 200 (`match`) or differed (`mismatch`).
        pub fn record_feed_forced_refresh(&self, outcome: &'static str) {
            let key = Key::from_parts(
                "kiki_feed_forced_refresh_total",
                vec![Label::new("outcome", outcome)],
            );
            self.recorder.register_counter(&key, &METADATA).increment(1);
        }

        /// Record a detected instance of a server returning unchanged
        /// `ETag`/`Last-Modified` alongside a different body — i.e. the
        /// validators are lying.
        pub fn record_feed_validator_lie(&self) {
            let key = Key::from_name("kiki_feed_validator_lie_total");
            self.recorder.register_counter(&key, &METADATA).increment(1);
        }

        /// Record the gap, in seconds, between now and the scheduled next
        /// fetch for a feed. `source` identifies why the schedule was
        /// chosen (`cache_hint`, `retry_after`, `backoff`, or `permanent`).
        pub fn record_feed_retry_scheduled(&self, source: &'static str, seconds_until: f64) {
            let key = Key::from_parts(
                "kiki_feed_retry_scheduled_seconds",
                vec![Label::new("source", source)],
            );
            self.recorder
                .register_histogram(&key, &METADATA)
                .record(seconds_until.max(0.0));
        }

        /// Record the length of the consecutive-failure streak at the moment
        /// a transient error was persisted.
        pub fn record_feed_consecutive_failures(&self, count: u32) {
            let key = Key::from_name("kiki_feed_consecutive_failures");
            self.recorder
                .register_histogram(&key, &METADATA)
                .record(count as f64);
        }

        // ----- Task queue -----

        pub fn record_task_enqueued(&self, task_type: &'static str) {
            let key = Key::from_parts(
                "kiki_tasks_enqueued_total",
                vec![Label::new("type", task_type)],
            );
            self.recorder.register_counter(&key, &METADATA).increment(1);
        }

        pub fn record_task_processed(
            &self,
            task_type: &'static str,
            outcome: &'static str,
            duration_seconds: f64,
        ) {
            let key = Key::from_parts(
                "kiki_tasks_processed_total",
                vec![
                    Label::new("type", task_type),
                    Label::new("outcome", outcome),
                ],
            );
            self.recorder.register_counter(&key, &METADATA).increment(1);

            let key = Key::from_parts(
                "kiki_task_duration_seconds",
                vec![Label::new("type", task_type)],
            );
            self.recorder
                .register_histogram(&key, &METADATA)
                .record(duration_seconds);
        }

        pub fn set_task_queue_depth(&self, depth: f64) {
            let key = Key::from_name("kiki_task_queue_depth");
            self.recorder.register_gauge(&key, &METADATA).set(depth);
        }

        pub fn set_workers_total(&self, total: f64) {
            let key = Key::from_name("kiki_workers_total");
            self.recorder.register_gauge(&key, &METADATA).set(total);
        }

        pub fn inc_workers_busy(&self) {
            let key = Key::from_name("kiki_workers_busy");
            self.recorder.register_gauge(&key, &METADATA).increment(1.0);
        }

        pub fn dec_workers_busy(&self) {
            let key = Key::from_name("kiki_workers_busy");
            self.recorder.register_gauge(&key, &METADATA).decrement(1.0);
        }

        pub fn set_feeds_refresh_in_progress(&self, n: f64) {
            let key = Key::from_name("kiki_feeds_refresh_in_progress");
            self.recorder.register_gauge(&key, &METADATA).set(n);
        }

        // ----- Database pool -----

        pub fn set_db_pool_state(&self, total: f64, idle: f64) {
            let key = Key::from_name("kiki_db_pool_connections");
            self.recorder.register_gauge(&key, &METADATA).set(total);

            let key = Key::from_name("kiki_db_pool_connections_idle");
            self.recorder.register_gauge(&key, &METADATA).set(idle);

            let key = Key::from_name("kiki_db_pool_connections_in_use");
            self.recorder
                .register_gauge(&key, &METADATA)
                .set((total - idle).max(0.0));
        }

        pub fn record_db_pool_acquire(&self, duration_seconds: f64, ok: bool) {
            let key = Key::from_name("kiki_db_pool_acquire_duration_seconds");
            self.recorder
                .register_histogram(&key, &METADATA)
                .record(duration_seconds);
            if !ok {
                let key = Key::from_name("kiki_db_pool_acquire_errors_total");
                self.recorder.register_counter(&key, &METADATA).increment(1);
            }
        }

        // ----- Retention -----

        pub fn record_retention_cleanup(
            &self,
            scope: &'static str,
            duration_seconds: f64,
            deleted: u64,
        ) {
            let key = Key::from_parts(
                "kiki_retention_cleanup_duration_seconds",
                vec![Label::new("scope", scope)],
            );
            self.recorder
                .register_histogram(&key, &METADATA)
                .record(duration_seconds);

            let key = Key::from_parts(
                "kiki_entries_deleted_total",
                vec![Label::new("scope", scope)],
            );
            self.recorder
                .register_counter(&key, &METADATA)
                .increment(deleted);
        }

        // ----- Domain totals -----

        pub fn set_feeds_total(&self, n: f64) {
            let key = Key::from_name("kiki_feeds_total");
            self.recorder.register_gauge(&key, &METADATA).set(n);
        }

        pub fn set_entries_total(&self, n: f64) {
            let key = Key::from_name("kiki_entries_total");
            self.recorder.register_gauge(&key, &METADATA).set(n);
        }

        pub fn set_feeds_with_fetch_error(&self, n: f64) {
            let key = Key::from_name("kiki_feeds_with_fetch_error");
            self.recorder.register_gauge(&key, &METADATA).set(n);
        }

        // ----- Plugins -----

        pub fn set_plugins_loaded(&self, n: f64) {
            let key = Key::from_name("kiki_plugins_loaded");
            self.recorder.register_gauge(&key, &METADATA).set(n);
        }

        pub fn record_plugin_load_error(&self) {
            let key = Key::from_name("kiki_plugin_load_errors_total");
            self.recorder.register_counter(&key, &METADATA).increment(1);
        }

        pub fn record_plugin_execution(&self, duration_seconds: f64, outcome: &'static str) {
            let key = Key::from_parts(
                "kiki_plugin_executions_total",
                vec![Label::new("outcome", outcome)],
            );
            self.recorder.register_counter(&key, &METADATA).increment(1);

            let key = Key::from_name("kiki_plugin_execution_duration_seconds");
            self.recorder
                .register_histogram(&key, &METADATA)
                .record(duration_seconds);
        }
    }

    /// Axum middleware that records per-request metrics. Skips `/metrics`
    /// itself so scrape traffic does not dominate the counters.
    pub async fn track_http(
        State(metrics): State<Arc<Metrics>>,
        req: Request<Body>,
        next: Next,
    ) -> Response {
        let path = req
            .extensions()
            .get::<MatchedPath>()
            .map(|m| m.as_str().to_string())
            .unwrap_or_else(|| req.uri().path().to_string());

        if path == "/metrics" {
            return next.run(req).await;
        }

        let method = req.method().as_str().to_string();
        let in_flight = metrics.http_in_flight(&method, &path);
        in_flight.increment(1.0);

        let start = Instant::now();
        let response = next.run(req).await;
        let elapsed = start.elapsed().as_secs_f64();

        in_flight.decrement(1.0);

        let status = response.status().as_u16().to_string();
        metrics.inc_http_requests(&method, &path, &status);
        metrics.record_http_duration(&method, &path, elapsed);

        response
    }

    /// Axum handler for `GET /metrics`.
    pub async fn handle_metrics(State(state): State<AppState>) -> Response {
        let body = state.metrics.render();
        let mut response = body.into_response();
        response.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
        );
        response
    }
}

#[cfg(not(feature = "metrics"))]
mod stub {
    /// No-op stand-in for the real [`Metrics`] struct used when the `metrics`
    /// feature is disabled. All methods compile to zero-overhead no-ops.
    pub struct Metrics;

    impl Metrics {
        pub fn new() -> anyhow::Result<Self> {
            Ok(Self)
        }

        pub fn render(&self) -> String {
            String::new()
        }

        // ----- Feed fetching -----

        #[inline]
        pub fn record_feed_fetch(&self, _outcome: &'static str, _duration_seconds: f64) {}
        #[inline]
        pub fn record_feed_response_bytes(&self, _bytes: u64) {}
        #[inline]
        pub fn record_feed_redirects(&self, _hops: u64) {}
        #[inline]
        pub fn record_feed_parse(
            &self,
            _format: &'static str,
            _duration_seconds: f64,
            _entries: u64,
        ) {
        }
        #[inline]
        pub fn record_feed_entry_upserted(&self, _format: &'static str) {}
        #[inline]
        pub fn record_feed_cache_hit(&self, _reason: &'static str) {}
        #[inline]
        pub fn record_feed_forced_refresh(&self, _outcome: &'static str) {}
        #[inline]
        pub fn record_feed_validator_lie(&self) {}
        #[inline]
        pub fn record_feed_retry_scheduled(&self, _source: &'static str, _seconds_until: f64) {}
        #[inline]
        pub fn record_feed_consecutive_failures(&self, _count: u32) {}

        // ----- Task queue -----

        #[inline]
        pub fn record_task_enqueued(&self, _task_type: &'static str) {}
        #[inline]
        pub fn record_task_processed(
            &self,
            _task_type: &'static str,
            _outcome: &'static str,
            _duration_seconds: f64,
        ) {
        }
        #[inline]
        pub fn set_task_queue_depth(&self, _depth: f64) {}
        #[inline]
        pub fn set_workers_total(&self, _total: f64) {}
        #[inline]
        pub fn inc_workers_busy(&self) {}
        #[inline]
        pub fn dec_workers_busy(&self) {}
        #[inline]
        pub fn set_feeds_refresh_in_progress(&self, _n: f64) {}

        // ----- Database pool -----

        #[inline]
        pub fn set_db_pool_state(&self, _total: f64, _idle: f64) {}
        #[inline]
        pub fn record_db_pool_acquire(&self, _duration_seconds: f64, _ok: bool) {}

        // ----- Retention -----

        #[inline]
        pub fn record_retention_cleanup(
            &self,
            _scope: &'static str,
            _duration_seconds: f64,
            _deleted: u64,
        ) {
        }

        // ----- Domain totals -----

        #[inline]
        pub fn set_feeds_total(&self, _n: f64) {}
        #[inline]
        pub fn set_entries_total(&self, _n: f64) {}
        #[inline]
        pub fn set_feeds_with_fetch_error(&self, _n: f64) {}

        // ----- Plugins -----

        #[inline]
        pub fn set_plugins_loaded(&self, _n: f64) {}
        #[inline]
        pub fn record_plugin_load_error(&self) {}
        #[inline]
        pub fn record_plugin_execution(&self, _duration_seconds: f64, _outcome: &'static str) {}
    }
}
