use crate::server::AppState;
use crate::server::ComponentState;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::{event, Level};

#[derive(Serialize, Deserialize, utoipa::ToSchema)]
pub struct HealthResponse {
    /// `"ok"`; `"degraded"` when something besides fetching feeds is not
    /// working, such as scripts or writing to the database; or
    /// `"unavailable"` when feeds can no longer be fetched.
    pub status: String,
    pub feed_count: usize,
    pub entry_count: usize,
    /// The isolated process feeds are fetched in.
    pub feed_fetcher: ComponentState,
    /// The isolated process plugins' scripts run in.
    pub script_host: ComponentState,
    /// How many task workers, which refresh feeds among other things, are
    /// still running.
    pub workers: usize,
    /// Whether the database took a write within [`WRITE_PROBE_TIMEOUT`].
    pub database_writable: bool,
}

/// How long the health check waits for the database's writer.
pub const WRITE_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Health check
///
/// Returns a 200 response if the server is live and able to fetch feeds,
/// whether or not everything else is working, and a 503 response if it is
/// not. The body says which of the server's parts are working.
#[utoipa::path(
    get,
    path = "/v1/health",
    responses(
        (status = 200, description = "Service is healthy, or degraded", body = HealthResponse),
        (status = 503, description = "Service unavailable", body = HealthResponse),
    ),
    tag = "meta"
)]
#[axum::debug_handler]
pub async fn health(State(state): State<AppState>) -> Result<Response, Response> {
    let result = state
        .db
        .read(move |conn| {
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

            Ok::<(usize, usize), rusqlite::Error>((feed_count, entry_count))
        })
        .await
        .inspect_err(|e| {
            event!(Level::ERROR, "task error in health: {:?}", e);
        });

    let Ok(Ok((feed_count, entry_count))) = result else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "Health check failed").into_response());
    };

    // Only takes the writer and gives it back, but that shows it is not
    // stuck behind anything.
    let database_writable = matches!(
        tokio::time::timeout(WRITE_PROBE_TIMEOUT, state.db.write(|_| ())).await,
        Ok(Ok(()))
    );
    let liveness = &state.liveness;
    let feed_fetcher = liveness.feed_fetcher();
    let script_host = liveness.script_host();
    let (code, status) = if !liveness.is_serviceable() {
        (StatusCode::SERVICE_UNAVAILABLE, "unavailable")
    } else if script_host == ComponentState::Gone || !database_writable {
        (StatusCode::OK, "degraded")
    } else {
        (StatusCode::OK, "ok")
    };
    let body = HealthResponse {
        status: status.to_string(),
        feed_count,
        entry_count,
        feed_fetcher,
        script_host,
        workers: liveness.workers(),
        database_writable,
    };
    let response = (code, Json(body)).into_response();
    if code == StatusCode::OK {
        Ok(response)
    } else {
        Err(response)
    }
}
