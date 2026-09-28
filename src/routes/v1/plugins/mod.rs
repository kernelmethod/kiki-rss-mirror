pub mod get_plugin;
pub mod list_plugins;
pub mod plugin_config;

use crate::server::AppState;
use axum::{
    routing::{delete, get},
    Router,
};
use get_plugin::get_plugin;
use list_plugins::list_plugins;

pub fn create_router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_plugins))
        .route("/name/{name}", get(get_plugin))
        .route(
            "/name/{name}/config",
            get(plugin_config::get_plugin_config)
                .put(plugin_config::put_plugin_config)
                .patch(plugin_config::patch_plugin_config)
                .delete(plugin_config::delete_plugin_config),
        )
        .route(
            "/name/{name}/config/{key}",
            delete(plugin_config::delete_plugin_config_key),
        )
}
