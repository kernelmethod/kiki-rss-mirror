use crate::serve::server::AppState;
use axum::{extract::State, http::StatusCode};

/// Route handler for listing all of the available feeds.
#[axum::debug_handler]
pub async fn list_feeds(State(_state): State<AppState>) -> (StatusCode, &'static str) {
    (StatusCode::OK, "")
}
