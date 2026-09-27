pub mod assets;
pub mod feed_fetch;
pub mod retention;

#[cfg(test)]
mod tests;

use crate::config::{ConfigError, Overrides, Settings};
use crate::server::AppState;
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use std::sync::Arc;
use tokio::task;
use tracing::{event, Level};

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

/// Applies `edit` to the config file through the server's
/// [`crate::config::ConfigStore`] and returns the new settings.
///
/// An edit producing an invalid config is answered with `400 Bad Request`
/// and the reason. A config file on disk that is itself invalid — say, a
/// broken hand edit — is answered with `409 Conflict`, since no request
/// can succeed until the file is fixed. Any other failure (the file cannot
/// be read or saved) is a `500 Internal Server Error`. Either way, nothing
/// changes.
async fn update_config<F>(state: &AppState, edit: F) -> Result<Arc<Settings>, Response>
where
    F: FnOnce(&mut Overrides) -> Result<(), ConfigError> + Send + 'static,
{
    let config = state.config.clone();
    match task::spawn_blocking(move || config.update(edit)).await {
        Ok(Ok(settings)) => Ok(settings),
        Ok(Err(ConfigError::Invalid(msg))) => Err((StatusCode::BAD_REQUEST, msg).into_response()),
        Ok(Err(e @ (ConfigError::InvalidFile { .. } | ConfigError::Parse { .. }))) => {
            let e = anyhow::Error::from(e);
            event!(Level::WARN, "refusing to update config: {:#}", e);
            Err((
                StatusCode::CONFLICT,
                format!("the config file on disk is invalid; fix or remove it first: {e:#}"),
            )
                .into_response())
        }
        Ok(Err(e)) => {
            event!(
                Level::ERROR,
                "failed to update config: {:#}",
                anyhow::Error::from(e)
            );
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(e) => {
            event!(Level::ERROR, "task error while updating config: {:?}", e);
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
    }
}
