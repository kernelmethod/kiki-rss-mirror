//! Settings endpoint for how feed responses are fetched.
use crate::db::settings as db_settings;
use crate::server::AppState;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use tokio::task;
use tracing::{event, Level};

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct FeedFetchSettingsResponse {
    /// Largest feed response body, in bytes, that will be read into memory.
    pub max_feed_bytes: u64,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct FeedFetchSettingsRequest {
    /// Largest feed response body, in bytes, to read into memory. Must be
    /// greater than zero. Leave `null` to keep the current value.
    pub max_feed_bytes: Option<u64>,
}

fn read_settings(conn: &rusqlite::Connection) -> anyhow::Result<FeedFetchSettingsResponse> {
    Ok(FeedFetchSettingsResponse {
        max_feed_bytes: db_settings::get_max_feed_bytes(conn)?,
    })
}

/// Get feed fetch settings.
#[utoipa::path(
    get,
    path = "/v1/settings/feed-fetch",
    responses(
        (status = 200, description = "Current feed fetch settings",
         body = FeedFetchSettingsResponse),
        (status = 500, description = "Internal server error"),
    ),
    tag = "settings"
)]
#[axum::debug_handler]
pub async fn get_feed_fetch_settings(State(state): State<AppState>) -> Result<Response, Response> {
    let pool = state.conn_pool.clone();
    let res = task::spawn_blocking(move || -> anyhow::Result<FeedFetchSettingsResponse> {
        let conn = pool.get()?;
        read_settings(&conn)
    })
    .await;

    match res {
        Ok(Ok(s)) => Ok(Json(s).into_response()),
        Ok(Err(e)) => {
            event!(Level::ERROR, "read feed fetch settings: {:?}", e);
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(e) => {
            event!(
                Level::ERROR,
                "task error in get feed fetch settings: {:?}",
                e
            );
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
    }
}

/// Update feed fetch settings. Any field left `null` is unchanged.
///
/// The new cap applies to the next fetch of every feed; no restart is
/// needed. Lowering it does not retroactively affect already-stored
/// entries.
#[utoipa::path(
    put,
    path = "/v1/settings/feed-fetch",
    request_body = FeedFetchSettingsRequest,
    responses(
        (status = 200, description = "Updated feed fetch settings",
         body = FeedFetchSettingsResponse),
        (status = 400, description = "Invalid value"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "settings"
)]
#[axum::debug_handler]
pub async fn put_feed_fetch_settings(
    State(state): State<AppState>,
    Json(payload): Json<FeedFetchSettingsRequest>,
) -> Result<Response, Response> {
    // A cap of zero would reject every feed, so refuse it at the boundary
    // rather than letting the operator lock themselves out of all fetches.
    if payload.max_feed_bytes == Some(0) {
        return Err((
            StatusCode::BAD_REQUEST,
            "max_feed_bytes must be greater than zero",
        )
            .into_response());
    }

    let pool = state.conn_pool.clone();
    let res = task::spawn_blocking(move || -> anyhow::Result<FeedFetchSettingsResponse> {
        let conn = pool.get()?;
        if let Some(max_feed_bytes) = payload.max_feed_bytes {
            db_settings::set_max_feed_bytes(&conn, max_feed_bytes)?;
        }
        read_settings(&conn)
    })
    .await;

    match res {
        Ok(Ok(s)) => Ok(Json(s).into_response()),
        Ok(Err(e)) => {
            event!(Level::ERROR, "write feed fetch settings: {:?}", e);
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(e) => {
            event!(
                Level::ERROR,
                "task error in put feed fetch settings: {:?}",
                e
            );
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
    }
}
