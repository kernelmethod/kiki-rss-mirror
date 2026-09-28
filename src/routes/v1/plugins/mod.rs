pub mod get_plugin;
pub mod list_plugins;
pub mod reload_plugins;

use crate::server::AppState;
use axum::{
    routing::{get, post},
    Router,
};
use get_plugin::get_plugin;
use list_plugins::list_plugins;
use reload_plugins::reload_plugins;

pub fn create_router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_plugins))
        .route("/name/{name}", get(get_plugin))
        .route("/reload", post(reload_plugins))
}
