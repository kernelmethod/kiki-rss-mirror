pub mod get_plugin;
pub mod list_plugins;

use crate::server::AppState;
use axum::{routing::get, Router};
use get_plugin::get_plugin;
use list_plugins::list_plugins;

pub fn create_router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_plugins))
        .route("/name/{name}", get(get_plugin))
}
