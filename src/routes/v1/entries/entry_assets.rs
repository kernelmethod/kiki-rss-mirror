//! `GET /v1/entries/id/{id}/assets` — list the cached asset mapping for an
//! entry so that clients can rewrite `<img src>` URLs when rendering.
use crate::db::assets as db_assets;
use crate::server::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use tracing::{event, Level};

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct EntryAsset {
    pub original_url: String,
    pub blake3: String,
    /// Relative Kiki URL that serves the cached bytes.
    pub url: String,
    pub content_type: Option<String>,
    pub size_bytes: i64,
    /// `"inline_img"` for images parsed out of entry content, or
    /// `"enclosure"` for RSS/Atom declared enclosures.
    pub kind: String,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ListEntryAssetsResponse {
    pub assets: Vec<EntryAsset>,
}

/// List cached assets for an entry.
#[utoipa::path(
    get,
    path = "/v1/entries/id/{id}/assets",
    params(("id" = i64, Path, description = "Entry ID")),
    responses(
        (status = 200, description = "Cached assets for the entry",
         body = ListEntryAssetsResponse),
        (status = 404, description = "Entry not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "entries"
)]
#[axum::debug_handler]
pub async fn list_entry_assets(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, Response> {
    let db = state.db.clone();
    let res = db
        .read(
            move |conn| -> anyhow::Result<Option<Vec<db_assets::EntryAssetRow>>> {
                let exists: Option<i64> = conn
                    .query_row("SELECT id FROM entries WHERE id = ?1", [id], |row| {
                        row.get(0)
                    })
                    .ok();
                match exists {
                    Some(_) => Ok(Some(db_assets::list_entry_assets(conn, id)?)),
                    None => Ok(None),
                }
            },
        )
        .await;

    match res {
        Ok(Ok(Some(rows))) => {
            let assets = rows
                .into_iter()
                .map(|r| EntryAsset {
                    original_url: r.asset.original_url,
                    url: crate::routes::v1::assets::asset_url(&r.asset.blake3),
                    blake3: r.asset.blake3,
                    content_type: r.asset.content_type,
                    size_bytes: r.asset.size_bytes,
                    kind: r.kind,
                })
                .collect::<Vec<_>>();
            Ok(Json(ListEntryAssetsResponse { assets }).into_response())
        }
        Ok(Ok(None)) => Err((StatusCode::NOT_FOUND, "Entry not found").into_response()),
        Ok(Err(e)) => {
            event!(Level::ERROR, "db error in list_entry_assets: {:?}", e);
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(e) => {
            event!(Level::ERROR, "task error in list_entry_assets: {:?}", e);
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
    }
}
