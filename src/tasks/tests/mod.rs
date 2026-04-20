#[cfg(feature = "lua")]
mod lua_script_tests;

mod backoff_tests;
mod cache_tests;
mod fetch_error_tests;
mod format_data_tests;
mod maintenance_tests;
mod retry_after_tests;
#[cfg(feature = "extra-tests")]
mod stress_tests;

/// Build a throwaway [`Metrics`] recorder for tests that need to call
/// instrumented code paths without caring about the emitted samples.
#[allow(dead_code, clippy::expect_used)]
pub(crate) fn test_metrics() -> crate::metrics::Metrics {
    crate::metrics::Metrics::new().expect("build test metrics")
}
