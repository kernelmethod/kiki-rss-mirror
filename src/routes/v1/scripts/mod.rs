pub mod add_script;
pub mod delete_script;
pub mod get_script;
pub mod list_scripts;
pub mod reload_scripts;
pub mod update_script;

use crate::server::AppState;
use add_script::add_script;
use axum::{
    routing::{get, post},
    Router,
};
use delete_script::delete_script;
use get_script::get_script;
use list_scripts::list_scripts;
use reload_scripts::reload_scripts;
use update_script::update_script;

pub fn create_router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_scripts))
        .route("/create", post(add_script))
        .route(
            "/id/{id}",
            get(get_script).put(update_script).delete(delete_script),
        )
        .route("/reload", post(reload_scripts))
}
