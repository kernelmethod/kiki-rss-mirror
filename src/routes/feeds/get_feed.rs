use crate::serve::server::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
};
use std::sync::Arc;

/// Route handler for fetching a single feed's information.
#[axum::debug_handler]
pub async fn get_feed(
    State(_state): State<Arc<AppState>>,
    Path(_id): Path<u64>,
) -> (StatusCode, &'static str) {
    (StatusCode::OK, "")
}
