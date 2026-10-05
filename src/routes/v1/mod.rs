pub mod access;
pub mod assets;
pub mod docs;
pub mod entries;
pub mod feeds;
pub mod health;
pub mod plugins;
pub mod root;
pub mod settings;
pub mod shutdown;
pub mod tags;
pub mod tokens;

use crate::server::AppState;
use axum::{
    routing::{get, post},
    Router,
};

pub fn create_router() -> Router<AppState> {
    Router::new()
        .route("/", get(root::root))
        .route("/health", get(health::health))
        .route("/access", get(access::access))
        .route("/shutdown", post(shutdown::shutdown))
        .nest("/assets", assets::create_router())
        .nest("/feeds", feeds::create_router())
        .nest("/entries", entries::create_router())
        .nest("/plugins", plugins::create_router())
        .nest("/settings", settings::create_router())
        .nest("/tags", tags::create_router())
        .nest("/tokens", tokens::create_router())
}
