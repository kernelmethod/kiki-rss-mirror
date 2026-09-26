//! Settings endpoint for the feed asset cache.
use crate::db::assets as db_assets;
use crate::server::AppState;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use tokio::task;
use tracing::{event, Level};

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

fn read_settings(conn: &rusqlite::Connection) -> anyhow::Result<AssetCacheSettingsResponse> {
    Ok(AssetCacheSettingsResponse {
        enabled: db_assets::get_cache_enabled(conn)?,
        max_bytes: db_assets::get_cache_max_bytes(conn)?,
        current_bytes: db_assets::total_cache_size(conn)?,
    })
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
    let pool = state.conn_pool.clone();
    let res = task::spawn_blocking(move || -> anyhow::Result<AssetCacheSettingsResponse> {
        let conn = pool.get()?;
        read_settings(&conn)
    })
    .await;

    match res {
        Ok(Ok(s)) => Ok(Json(s).into_response()),
        Ok(Err(e)) => {
            event!(Level::ERROR, "read asset cache settings: {:?}", e);
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(e) => {
            event!(
                Level::ERROR,
                "task error in get asset cache settings: {:?}",
                e
            );
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
    }
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
        (status = 500, description = "Internal server error"),
    ),
    tag = "settings"
)]
#[axum::debug_handler]
pub async fn put_asset_cache_settings(
    State(state): State<AppState>,
    Json(payload): Json<AssetCacheSettingsRequest>,
) -> Result<Response, Response> {
    if let Some(b) = payload.max_bytes {
        if b < 0 {
            return Err((StatusCode::BAD_REQUEST, "max_bytes must be non-negative").into_response());
        }
    }

    let pool = state.conn_pool.clone();
    let res = task::spawn_blocking(move || -> anyhow::Result<AssetCacheSettingsResponse> {
        let conn = pool.get()?;
        if let Some(enabled) = payload.enabled {
            db_assets::set_cache_enabled(&conn, enabled)?;
        }
        if let Some(max_bytes) = payload.max_bytes {
            db_assets::set_cache_max_bytes(&conn, max_bytes)?;
        }
        read_settings(&conn)
    })
    .await;

    match res {
        Ok(Ok(s)) => Ok(Json(s).into_response()),
        Ok(Err(e)) => {
            event!(Level::ERROR, "write asset cache settings: {:?}", e);
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(e) => {
            event!(
                Level::ERROR,
                "task error in put asset cache settings: {:?}",
                e
            );
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
    }
}
