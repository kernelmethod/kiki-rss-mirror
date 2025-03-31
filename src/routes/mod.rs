mod feeds;
mod root;

use crate::server::AppState;
use axum::{routing::get, Router};
use std::time::Duration;
use tower_http::{timeout::TimeoutLayer, trace::TraceLayer};

pub fn create_router() -> Router<AppState> {
    Router::new()
        .route("/", get(root::root))
        .nest("/feeds", feeds::create_router())
        .layer((
            TraceLayer::new_for_http(),
            TimeoutLayer::new(Duration::from_secs(10)),
        ))
}
