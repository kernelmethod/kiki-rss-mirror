use crate::server::AppState;
use axum::{extract::State, http::StatusCode, response::IntoResponse};
use tracing::error;

/// Route handler that rebuilds the global [`LuaScriptRunner`] from the latest scripts in
/// the database.
///
/// After this endpoint is called the fetcher manager replaces the cached runner with a
/// freshly compiled one, picking up any script additions, removals, or edits.
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
    use crate::fetcher::FetchManagerCommand;

    if let Err(e) = state
        .fetcher_tx
        .send(FetchManagerCommand::ReloadScripts)
        .await
    {
        error!("failed to send ReloadScripts command: {:?}", e);
        return StatusCode::INTERNAL_SERVER_ERROR;
    }
    StatusCode::ACCEPTED
}
