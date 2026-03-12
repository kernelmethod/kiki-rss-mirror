pub mod list_scripts;
pub mod reload_scripts;

use crate::server::AppState;
use axum::{
    routing::{get, post},
    Router,
};
use list_scripts::list_scripts;
use reload_scripts::reload_scripts;

pub fn create_router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_scripts))
        .route("/reload", post(reload_scripts))
}
