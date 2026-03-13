pub mod retention;

use crate::server::AppState;
use axum::{routing::get, Router};

pub fn create_router() -> Router<AppState> {
    Router::new().route(
        "/retention",
        get(retention::get_retention).put(retention::put_retention),
    )
}
