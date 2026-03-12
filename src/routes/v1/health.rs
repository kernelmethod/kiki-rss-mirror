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

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub feed_count: usize,
    pub entry_count: usize,
}

/// Route handler for the health check endpoint.
#[axum::debug_handler]
pub async fn health(State(state): State<AppState>) -> Result<Response, Response> {
    let conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        (StatusCode::SERVICE_UNAVAILABLE, "Database unavailable").into_response()
    })?;

    let result = task::spawn_blocking(move || {
        let feed_count: usize = conn
            .prepare("SELECT COUNT(*) FROM feeds")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row([], |row| row.get(0))?;

        let entry_count: usize = conn
            .prepare("SELECT COUNT(*) FROM entries")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row([], |row| row.get(0))?;

        Ok::<HealthResponse, rusqlite::Error>(HealthResponse {
            status: "ok".to_string(),
            feed_count,
            entry_count,
        })
    })
    .await
    .inspect_err(|e| {
        event!(Level::ERROR, "task error in health: {:?}", e);
    });

    match result {
        Ok(Ok(response)) => Ok(Json(response).into_response()),
        _ => Err((StatusCode::SERVICE_UNAVAILABLE, "Health check failed").into_response()),
    }
}
