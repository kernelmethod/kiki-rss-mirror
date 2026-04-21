//! Routes for serving and managing cached feed assets.
use crate::db::assets as db_assets;
use crate::server::AppState;
use crate::tasks::assets as task_assets;
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Router,
};
use serde::Deserialize;
use tokio::task;
use tracing::{event, Level};

#[cfg(test)]
mod tests;

pub fn create_router() -> Router<AppState> {
    Router::new()
        .route("/by-url", get(get_asset_by_url))
        .route("/{hash}", get(get_asset).delete(delete_asset))
}

fn is_hex_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `GET /v1/assets/{hash}` — serve the cached bytes for a blake3 hash.
///
/// Responds with `304 Not Modified` when the client's `If-None-Match`
/// matches the stored hash.
#[utoipa::path(
    get,
    path = "/v1/assets/{hash}",
    params(("hash" = String, Path, description = "blake3 hex hash of the cached asset")),
    responses(
        (status = 200, description = "Cached asset bytes"),
        (status = 304, description = "Client already has the asset"),
        (status = 400, description = "Hash is not a 64-char hex string"),
        (status = 404, description = "Asset not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "assets"
)]
#[axum::debug_handler]
pub async fn get_asset(
    State(state): State<AppState>,
    Path(hash): Path<String>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    if !is_hex_hash(&hash) {
        return Err((StatusCode::BAD_REQUEST, "invalid hash").into_response());
    }

    let if_none_match = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim_matches('"').to_string());

    let pool = state.conn_pool.clone();
    let hash_cloned = hash.clone();
    let row = task::spawn_blocking(move || -> anyhow::Result<Option<db_assets::AssetRow>> {
        let conn = pool.get()?;
        let row = db_assets::lookup_by_hash(&conn, &hash_cloned)?;
        if row.is_some() {
            let _ = db_assets::touch(&conn, &hash_cloned);
        }
        Ok(row)
    })
    .await
    .map_err(|e| {
        event!(Level::ERROR, "task error in get_asset: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?
    .map_err(|e| {
        event!(Level::ERROR, "db error in get_asset: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;

    let asset = match row {
        Some(r) => r,
        None => return Err((StatusCode::NOT_FOUND, "asset not found").into_response()),
    };

    if if_none_match.as_deref() == Some(asset.blake3.as_str()) {
        return Ok(StatusCode::NOT_MODIFIED.into_response());
    }

    let path = task_assets::asset_path(&state.data_dir, &asset.blake3);
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) => {
            event!(
                Level::ERROR,
                "asset file missing for {}: {:?} ({})",
                asset.blake3,
                path,
                e
            );
            return Err((StatusCode::NOT_FOUND, "asset file not found").into_response());
        }
    };

    let content_type = asset
        .content_type
        .as_deref()
        .unwrap_or("application/octet-stream");
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            (header::ETAG, format!("\"{}\"", asset.blake3)),
            (
                header::CACHE_CONTROL,
                "public, max-age=604800, immutable".to_string(),
            ),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            (header::CONTENT_DISPOSITION, "inline".to_string()),
        ],
        bytes,
    )
        .into_response())
}

#[derive(Debug, Deserialize)]
pub struct ByUrlQuery {
    /// Original (pre-cache) asset URL.
    pub url: String,
}

/// `GET /v1/assets/by-url?url=<original>` — 302 redirect to the hash-addressed
/// endpoint when cached, 404 otherwise. Lets clients rewrite feed asset
/// URLs through Kiki without first listing the entry's assets.
#[utoipa::path(
    get,
    path = "/v1/assets/by-url",
    params(("url" = String, Query, description = "Original asset URL")),
    responses(
        (status = 302, description = "Redirect to /v1/assets/{hash}"),
        (status = 404, description = "Asset not found in cache"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "assets"
)]
#[axum::debug_handler]
pub async fn get_asset_by_url(
    State(state): State<AppState>,
    Query(q): Query<ByUrlQuery>,
) -> Result<Response, Response> {
    let pool = state.conn_pool.clone();
    let url = q.url.clone();
    let row = task::spawn_blocking(move || -> anyhow::Result<Option<db_assets::AssetRow>> {
        let conn = pool.get()?;
        db_assets::lookup_by_url(&conn, &url)
    })
    .await
    .map_err(|e| {
        event!(Level::ERROR, "task error in get_asset_by_url: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?
    .map_err(|e| {
        event!(Level::ERROR, "db error in get_asset_by_url: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;

    match row {
        Some(r) => Ok(Redirect::to(&format!("/v1/assets/{}", r.blake3)).into_response()),
        None => Err((StatusCode::NOT_FOUND, "asset not found").into_response()),
    }
}

/// `DELETE /v1/assets/{hash}` — evict a single asset from the cache (both
/// the DB row and its file on disk). Idempotent: unknown hashes return 404.
#[utoipa::path(
    delete,
    path = "/v1/assets/{hash}",
    params(("hash" = String, Path)),
    responses(
        (status = 204, description = "Asset evicted"),
        (status = 400, description = "Hash is not a 64-char hex string"),
        (status = 404, description = "Asset not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "assets"
)]
#[axum::debug_handler]
pub async fn delete_asset(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> Result<Response, Response> {
    if !is_hex_hash(&hash) {
        return Err((StatusCode::BAD_REQUEST, "invalid hash").into_response());
    }

    let pool = state.conn_pool.clone();
    let data_dir = state.data_dir.clone();
    let hash_cloned = hash.clone();
    let deleted = task::spawn_blocking(move || -> anyhow::Result<Option<db_assets::AssetRow>> {
        let conn = pool.get()?;
        db_assets::delete_by_hash(&conn, &hash_cloned)
    })
    .await
    .map_err(|e| {
        event!(Level::ERROR, "task error in delete_asset: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?
    .map_err(|e| {
        event!(Level::ERROR, "db error in delete_asset: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;

    match deleted {
        Some(row) => {
            task_assets::unlink_asset_file(&data_dir, &row.blake3);
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        None => Err((StatusCode::NOT_FOUND, "asset not found").into_response()),
    }
}
