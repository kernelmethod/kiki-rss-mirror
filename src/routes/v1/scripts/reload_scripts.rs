use crate::server::AppState;
use axum::{extract::State, http::StatusCode, response::IntoResponse};
use tracing::error;

/// Reload all scripts
///
/// Manually queue a reload of all scripts, picking up any script additions, removals or edits.
/// Calls to this endpoint are usually unnecessary, as a reload is cued after any changes to a
/// server's scripts.
#[utoipa::path(
    post,
    path = "/v1/scripts/reload",
    responses(
        (status = 202, description = "Script runner reload queued successfully"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "scripts"
)]
#[axum::debug_handler]
pub async fn reload_scripts(State(state): State<AppState>) -> impl IntoResponse {
    if let Err(e) = state.reload_tx.send(()) {
        error!("failed to send ReloadScripts signal: {:?}", e);
        return StatusCode::INTERNAL_SERVER_ERROR;
    }
    StatusCode::ACCEPTED
}
