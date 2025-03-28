pub mod feed;

use axum::Router;
use std::time::Duration;
use tower_http::{timeout::TimeoutLayer, trace::TraceLayer};

pub fn create_router() -> Router {
    Router::new().nest("/feed", feed::create_router()).layer((
        TraceLayer::new_for_http(),
        TimeoutLayer::new(Duration::from_secs(10)),
    ))
}
