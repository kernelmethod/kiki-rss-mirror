use crate::db::retention;
use crate::server::AppState;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use tokio::task;
use tracing::{event, Level};

#[derive(Serialize, utoipa::ToSchema)]
pub struct CleanupResponse {
    pub deleted_count: usize,
}

/// Manually trigger retention cleanup across all feeds.
///
/// Deletes entries older than the configured `retention_max_age_days`.
/// Returns `deleted_count: 0` if no retention policy is configured.
#[utoipa::path(
    post,
    path = "/v1/entries/cleanup",
    responses(
        (status = 200, description = "Cleanup completed", body = CleanupResponse),
        (status = 500, description = "Internal server error"),
    ),
    tag = "entries"
)]
#[axum::debug_handler]
pub async fn cleanup(State(state): State<AppState>) -> Result<Response, Response> {
    let conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;

    let result = task::spawn_blocking(move || retention::cleanup_all(&conn)).await;

    match result {
        Ok(Ok(deleted_count)) => Ok(Json(CleanupResponse { deleted_count }).into_response()),
        Ok(Err(e)) => {
            event!(Level::ERROR, "error during cleanup: {:?}", e);
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(e) => {
            event!(Level::ERROR, "task error in cleanup: {:?}", e);
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
    }
}
