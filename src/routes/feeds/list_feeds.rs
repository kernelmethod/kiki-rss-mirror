use crate::serve::server::AppState;
use axum::{extract::State, http::StatusCode};
use std::sync::Arc;

/// Route handler for listing all of the available feeds.
#[axum::debug_handler]
pub async fn list_feeds(State(_state): State<Arc<AppState>>) -> (StatusCode, &'static str) {
    (StatusCode::OK, "")
}
