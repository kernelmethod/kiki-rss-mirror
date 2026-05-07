use crate::db::retention as db_retention;
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

#[derive(Serialize, Deserialize, utoipa::ToSchema)]
pub struct RetentionResponse {
    pub max_age_days: Option<i64>,
}

#[derive(Serialize, Deserialize, utoipa::ToSchema)]
#[cfg_attr(feature = "mcp", derive(schemars::JsonSchema))]
pub struct RetentionRequest {
    pub max_age_days: Option<i64>,
}

/// Get retention policy
///
/// Retrieve settings for the current retention policy for RSS/Atom entries.
#[utoipa::path(
    get,
    path = "/v1/settings/retention",
    responses(
        (status = 200, description = "Current retention settings", body = RetentionResponse),
        (status = 500, description = "Internal server error"),
    ),
    tag = "settings"
)]
#[axum::debug_handler]
pub async fn get_retention(State(state): State<AppState>) -> Result<Response, Response> {
    let conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;

    let result = task::spawn_blocking(move || db_retention::get_max_age_days(&conn)).await;

    match result {
        Ok(Ok(max_age_days)) => Ok(Json(RetentionResponse { max_age_days }).into_response()),
        Ok(Err(e)) => {
            event!(Level::ERROR, "error reading retention settings: {:?}", e);
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(e) => {
            event!(Level::ERROR, "task error in get_retention: {:?}", e);
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
    }
}

/// Update retention policy
///
/// Update the settings for the retention policy for RSS/Atom entries.
#[utoipa::path(
    put,
    path = "/v1/settings/retention",
    request_body = RetentionRequest,
    responses(
        (status = 200, description = "Updated retention settings", body = RetentionResponse),
        (status = 500, description = "Internal server error"),
    ),
    tag = "settings"
)]
#[axum::debug_handler]
pub async fn put_retention(
    State(state): State<AppState>,
    Json(payload): Json<RetentionRequest>,
) -> Result<Response, Response> {
    let conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;

    let result = task::spawn_blocking(move || {
        db_retention::set_max_age_days(&conn, payload.max_age_days)?;
        db_retention::get_max_age_days(&conn)
    })
    .await;

    match result {
        Ok(Ok(max_age_days)) => Ok(Json(RetentionResponse { max_age_days }).into_response()),
        Ok(Err(e)) => {
            event!(Level::ERROR, "error updating retention settings: {:?}", e);
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(e) => {
            event!(Level::ERROR, "task error in put_retention: {:?}", e);
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
    }
}
