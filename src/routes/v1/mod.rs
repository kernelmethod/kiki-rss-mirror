pub mod docs;
pub mod entries;
pub mod feeds;
pub mod health;
pub mod root;
pub mod tags;

use crate::server::AppState;
use axum::{routing::get, Router};

pub fn create_router() -> Router<AppState> {
    Router::new()
        .route("/", get(root::root))
        .route("/health", get(health::health))
        .nest("/feeds", feeds::create_router())
        .nest("/entries", entries::create_router())
        .nest("/tags", tags::create_router())
}
