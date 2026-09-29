//! Settings endpoint for how feed responses are fetched.
use super::update_config;
use crate::config::FeedFetchSettings;
use crate::server::AppState;
use axum::{
    extract::State,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use tokio::task;
use tracing::{event, Level};

const SECTION: &str = "feed_fetch";

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct FeedFetchSettingsResponse {
    /// HTTP request timeout for a feed fetch, in seconds.
    pub timeout_seconds: u64,
    /// Minimum interval, in seconds, between polls of any one feed.
    pub min_polling_cadence_seconds: u64,
    /// Fetch interval, in seconds, given to newly added feeds.
    pub default_fetch_interval_seconds: u64,
    /// Cap, in seconds, on backoff after failed fetches.
    pub max_backoff_seconds: u64,
    /// How often, in seconds, to skip conditional headers and force a full
    /// fetch.
    pub force_refresh_after_seconds: u64,
    /// Largest feed response body, in bytes, that will be read into memory.
    pub max_feed_bytes: u64,
}

impl From<&FeedFetchSettings> for FeedFetchSettingsResponse {
    fn from(s: &FeedFetchSettings) -> Self {
        FeedFetchSettingsResponse {
            timeout_seconds: s.timeout_seconds,
            min_polling_cadence_seconds: s.min_polling_cadence_seconds,
            default_fetch_interval_seconds: s.default_fetch_interval_seconds,
            max_backoff_seconds: s.max_backoff_seconds,
            force_refresh_after_seconds: s.force_refresh_after_seconds,
            max_feed_bytes: s.max_feed_bytes,
        }
    }
}

/// Fields left `null` (or omitted) keep their current value.
#[derive(Debug, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct FeedFetchSettingsRequest {
    /// HTTP request timeout for a feed fetch, in seconds. Must be greater
    /// than zero.
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    /// Minimum interval, in seconds, between polls of any one feed.
    #[serde(default)]
    pub min_polling_cadence_seconds: Option<u64>,
    /// Fetch interval, in seconds, given to newly added feeds. Must be
    /// greater than zero. Feeds already added keep their own interval.
    #[serde(default)]
    pub default_fetch_interval_seconds: Option<u64>,
    /// Cap, in seconds, on backoff after failed fetches.
    #[serde(default)]
    pub max_backoff_seconds: Option<u64>,
    /// How often, in seconds, to skip conditional headers and force a full
    /// fetch.
    #[serde(default)]
    pub force_refresh_after_seconds: Option<u64>,
    /// Largest feed response body, in bytes, to read into memory. Must be
    /// greater than zero.
    #[serde(default)]
    pub max_feed_bytes: Option<u64>,
}

/// Get feed fetch settings.
#[utoipa::path(
    get,
    path = "/v1/settings/feed-fetch",
    responses(
        (status = 200, description = "Current feed fetch settings",
         body = FeedFetchSettingsResponse),
    ),
    tag = "settings"
)]
#[axum::debug_handler]
pub async fn get_feed_fetch_settings(State(state): State<AppState>) -> Response {
    let settings = state.config.current();
    Json(FeedFetchSettingsResponse::from(&settings.feed_fetch)).into_response()
}

/// Update feed fetch settings. Any field left `null` is unchanged.
///
/// Changes apply to the next fetch of every feed; no restart is needed.
/// Lowering `max_backoff_seconds` also brings forward any fetch already
/// scheduled further out than the new cap. Lowering `max_feed_bytes` does
/// not retroactively affect already-stored entries, and
/// `default_fetch_interval_seconds` applies only to feeds added afterwards.
#[utoipa::path(
    put,
    path = "/v1/settings/feed-fetch",
    request_body = FeedFetchSettingsRequest,
    responses(
        (status = 200, description = "Updated feed fetch settings",
         body = FeedFetchSettingsResponse),
        (status = 400, description = "Invalid value"),
        (status = 409, description = "The config file on disk is invalid"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "settings"
)]
#[axum::debug_handler]
pub async fn put_feed_fetch_settings(
    State(state): State<AppState>,
    Json(payload): Json<FeedFetchSettingsRequest>,
) -> Result<Response, Response> {
    let backoff_changed = payload.max_backoff_seconds.is_some();
    let settings = update_config(&state, move |o| {
        let fields = [
            ("timeout_seconds", payload.timeout_seconds),
            (
                "min_polling_cadence_seconds",
                payload.min_polling_cadence_seconds,
            ),
            (
                "default_fetch_interval_seconds",
                payload.default_fetch_interval_seconds,
            ),
            ("max_backoff_seconds", payload.max_backoff_seconds),
            (
                "force_refresh_after_seconds",
                payload.force_refresh_after_seconds,
            ),
            ("max_feed_bytes", payload.max_feed_bytes),
        ];
        for (key, value) in fields {
            if let Some(value) = value {
                o.set(SECTION, key, value)?;
            }
        }
        Ok(())
    })
    .await?;

    if backoff_changed {
        cap_scheduled_fetches(&state, settings.feed_fetch.max_backoff_seconds).await;
    }

    Ok(Json(FeedFetchSettingsResponse::from(&settings.feed_fetch)).into_response())
}

/// Bring forward every fetch scheduled more than `max_backoff_seconds`
/// from now, so a lowered backoff cap applies to feeds that are already
/// waiting and not only to their next failure.
///
/// The settings are saved by the time this runs, so a failure is logged
/// rather than reported: the new cap still applies from each feed's next
/// fetch.
async fn cap_scheduled_fetches(state: &AppState, max_backoff_seconds: u64) {
    let pool = state.conn_pool.clone();
    let latest = chrono::Utc::now()
        .timestamp()
        .saturating_add(i64::try_from(max_backoff_seconds).unwrap_or(i64::MAX));
    let result = task::spawn_blocking(move || {
        pool.get()?.execute(
            "UPDATE feeds SET next_fetch_at = ?1 WHERE next_fetch_at > ?1",
            [latest],
        )?;
        Ok::<(), anyhow::Error>(())
    })
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => event!(Level::WARN, "failed to apply the new backoff cap: {:#}", e),
        Err(e) => event!(
            Level::WARN,
            "task error applying the new backoff cap: {:?}",
            e
        ),
    }
}
