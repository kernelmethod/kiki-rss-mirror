pub mod assets;
pub mod feed_fetch;
pub mod retention;

#[cfg(test)]
mod tests;

use crate::server::AppState;
use axum::{routing::get, Router};

pub fn create_router() -> Router<AppState> {
    Router::new()
        .route(
            "/retention",
            get(retention::get_retention).put(retention::put_retention),
        )
        .route(
            "/asset-cache",
            get(assets::get_asset_cache_settings).put(assets::put_asset_cache_settings),
        )
        .route(
            "/feed-fetch",
            get(feed_fetch::get_feed_fetch_settings).put(feed_fetch::put_feed_fetch_settings),
        )
}
