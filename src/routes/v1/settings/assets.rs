//! Settings endpoint for the feed asset cache.
use super::update_config;
use crate::config::AssetCacheSettings;
use crate::db::assets as db_assets;
use crate::server::AppState;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use tracing::{event, Level};

const SECTION: &str = "asset_cache";

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AssetCacheSettingsResponse {
    pub enabled: bool,
    pub max_bytes: i64,
    pub current_bytes: i64,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AssetCacheSettingsRequest {
    pub enabled: Option<bool>,
    pub max_bytes: Option<i64>,
}

/// Builds the response from `settings` plus the cache's current size,
/// which lives in the database.
async fn respond(state: &AppState, settings: &AssetCacheSettings) -> Result<Response, Response> {
    let db = state.db.clone();
    let res = db
        .read(move |conn| -> anyhow::Result<i64> { db_assets::total_cache_size(conn) })
        .await;

    match res {
        Ok(Ok(current_bytes)) => Ok(Json(AssetCacheSettingsResponse {
            enabled: settings.enabled,
            max_bytes: settings.max_bytes,
            current_bytes,
        })
        .into_response()),
        Ok(Err(e)) => {
            event!(Level::ERROR, "read asset cache size: {:?}", e);
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(e) => {
            event!(Level::ERROR, "task error reading asset cache size: {:?}", e);
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
    }
}

/// Get asset cache settings.
#[utoipa::path(
    get,
    path = "/v1/settings/asset-cache",
    responses(
        (status = 200, description = "Current asset cache settings",
         body = AssetCacheSettingsResponse),
        (status = 500, description = "Internal server error"),
    ),
    tag = "settings"
)]
#[axum::debug_handler]
pub async fn get_asset_cache_settings(State(state): State<AppState>) -> Result<Response, Response> {
    let settings = state.config.current();
    respond(&state, &settings.asset_cache).await
}

/// Update asset cache settings. Any field left `null` is unchanged.
#[utoipa::path(
    put,
    path = "/v1/settings/asset-cache",
    request_body = AssetCacheSettingsRequest,
    responses(
        (status = 200, description = "Updated asset cache settings",
         body = AssetCacheSettingsResponse),
        (status = 400, description = "Invalid value"),
        (status = 409, description = "The config file on disk is invalid"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "settings"
)]
#[axum::debug_handler]
pub async fn put_asset_cache_settings(
    State(state): State<AppState>,
    Json(payload): Json<AssetCacheSettingsRequest>,
) -> Result<Response, Response> {
    let settings = update_config(&state, move |o| {
        if let Some(enabled) = payload.enabled {
            o.set(SECTION, "enabled", enabled)?;
        }
        if let Some(max_bytes) = payload.max_bytes {
            o.set(SECTION, "max_bytes", max_bytes)?;
        }
        Ok(())
    })
    .await?;

    respond(&state, &settings.asset_cache).await
}
