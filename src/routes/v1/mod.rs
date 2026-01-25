mod entries;
mod feeds;
mod root;

use crate::server::AppState;
use axum::{routing::get, Router};

pub fn create_router() -> Router<AppState> {
    Router::new()
        .route("/", get(root::root))
        .nest("/feeds", feeds::create_router())
        .nest("/entries", entries::create_router())
}
