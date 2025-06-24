mod v1;

use crate::server::AppState;
use axum::Router;
use std::time::Duration;
use tower_http::{timeout::TimeoutLayer, trace::TraceLayer};

pub fn create_router() -> Router<AppState> {
    Router::new().nest("/v1/", v1::create_router()).layer((
        TraceLayer::new_for_http(),
        TimeoutLayer::new(Duration::from_secs(10)),
    ))
}
