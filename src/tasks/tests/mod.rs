mod lua_script_tests;

mod adaptive_tests;
mod asset_cache_tests;
mod auth_tests;
mod backoff_tests;
mod cache_control_parse;
mod cache_tests;
mod favicon_tests;
mod feed_hints_tests;
mod fetch_error_tests;
mod format_data_tests;
mod maintenance_tests;
mod pool_tests;
mod retention_tests;
mod retry_after_tests;
mod server_hints_tests;
#[cfg(feature = "extra-tests")]
mod stress_tests;
mod worker_tests;

/// Build a throwaway [`Metrics`] recorder for tests that need to call
/// instrumented code paths without caring about the emitted samples.
#[allow(dead_code, clippy::expect_used)]
pub(crate) fn test_metrics() -> crate::metrics::Metrics {
    crate::metrics::Metrics::new().expect("build test metrics")
}

/// Build a throwaway task-manager sender for tests that call `refresh_feed`
/// without caring about enqueued follow-up tasks. The paired receiver is
/// dropped, so `try_send` fills to capacity and then fails silently — which
/// is the behaviour `refresh_feed` already treats as non-fatal.
#[allow(dead_code)]
pub(crate) fn test_tx() -> crate::tasks::TaskSender {
    let (tx, _rx) = async_channel::bounded(1024);
    tx.into()
}
