//! Settings endpoint for how feed responses are fetched.
use super::update_config;
use crate::config::FeedFetchSettings;
use crate::routes::v1::feeds::update_feed::unwind_adaptive_fetch;
use crate::server::AppState;
use axum::{
    extract::State,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
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
    /// Whether to back off from feeds whose short freshness hint keeps
    /// turning out to be unchanged, between the minimum polling cadence and
    /// each feed's own interval. Feeds can override it.
    pub adaptive_fetch: bool,
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
            adaptive_fetch: s.adaptive_fetch,
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
    /// Whether to back off from feeds whose short freshness hint keeps
    /// turning out to be unchanged. Feeds with their own setting keep it.
    #[serde(default)]
    pub adaptive_fetch: Option<bool>,
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
/// Turning `adaptive_fetch` off brings forward any fetch it had put off,
/// for every feed without its own setting.
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
    let was_adaptive = state.config.current().feed_fetch.adaptive_fetch;
    let adaptive_fetch = payload.adaptive_fetch;
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
        if let Some(adaptive_fetch) = adaptive_fetch {
            o.set(SECTION, "adaptive_fetch", adaptive_fetch)?;
        }
        Ok(())
    })
    .await?;

    if backoff_changed {
        cap_scheduled_fetches(&state, settings.feed_fetch.max_backoff_seconds).await;
    }
    if was_adaptive && !settings.feed_fetch.adaptive_fetch {
        unwind_server_adaptive_fetch(&state, settings.feed_fetch.min_polling_cadence_seconds).await;
    }

    Ok(Json(FeedFetchSettingsResponse::from(&settings.feed_fetch)).into_response())
}

/// Undo adaptive fetching for every feed that follows the server setting,
/// now that it has been turned off; see [`unwind_adaptive_fetch`].
///
/// As with [`cap_scheduled_fetches`], a failure is logged rather than
/// reported: each feed stops adapting from its next fetch regardless.
async fn unwind_server_adaptive_fetch(state: &AppState, min_cadence: u64) {
    let min_cadence = i64::try_from(min_cadence).unwrap_or(i64::MAX);
    let result = state
        .db
        .write(move |conn| {
            unwind_adaptive_fetch(conn, "adaptive_fetch IS NULL", [min_cadence])?;
            Ok::<(), anyhow::Error>(())
        })
        .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => event!(
            Level::WARN,
            "failed to bring forward adaptively delayed fetches: {:#}",
            e
        ),
        Err(e) => event!(
            Level::WARN,
            "task error bringing forward adaptively delayed fetches: {:?}",
            e
        ),
    }
}

/// Bring forward every fetch scheduled more than `max_backoff_seconds`
/// from now, so a lowered backoff cap applies to feeds that are already
/// waiting and not only to their next failure.
///
/// The settings are saved by the time this runs, so a failure is logged
/// rather than reported: the new cap still applies from each feed's next
/// fetch.
async fn cap_scheduled_fetches(state: &AppState, max_backoff_seconds: u64) {
    let db = state.db.clone();
    let latest = chrono::Utc::now()
        .timestamp()
        .saturating_add(i64::try_from(max_backoff_seconds).unwrap_or(i64::MAX));
    let result = db
        .write(move |conn| {
            conn.execute(
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
