use crate::server::AppState;
use axum::{
    extract::{Path, State},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use tokio::task;
use tracing::{event, Level};

#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
pub struct GetEntryResponse {
    pub id: i64,
    pub feed_id: Option<i64>,
    pub source_id: Option<i64>,
    /// Syndication format: "rss" or "atom".
    pub syndication_format: String,
    pub guid: String,
    /// Publication time in RFC3339 format.
    pub published_at: Option<String>,
    pub title: String,
    pub url: String,
    pub content: Option<String>,
}

/// Get entry content
///
/// Retrieve content and metadata for a single entry by its ID.
#[utoipa::path(
    get,
    path = "/v1/entries/id/{id}",
    params(
        ("id" = i64, Path, description = "Entry ID"),
    ),
    responses(
        (status = 200, description = "Entry found", body = GetEntryResponse),
        (status = 404, description = "Entry not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "entries"
)]
pub async fn get_entry(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, Response> {
    let conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "Internal server error",
        )
            .into_response()
    })?;

    let result = task::spawn_blocking(move || {
        let entry = conn
            .prepare(
                "SELECT id, feed_id, source_id, syndication_format,
                    guid, published_at, title, url, content
                FROM entries WHERE id = ?1 LIMIT 1",
            )
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row([id], |row| {
                Ok(GetEntryResponse {
                    id: row.get(0)?,
                    feed_id: row.get(1)?,
                    source_id: row.get(2)?,
                    syndication_format: row.get(3)?,
                    guid: row.get(4)?,
                    published_at: chrono::DateTime::from_timestamp_secs(row.get(5)?)
                        .map(|d| d.to_rfc3339()),
                    title: row.get(6)?,
                    url: row.get(7)?,
                    content: row.get(8)?,
                })
            })
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                _ => Err(e),
            })
            .inspect_err(|e| {
                event!(Level::ERROR, "failed to get entry: {:?}", e);
            })?;

        Ok::<Option<GetEntryResponse>, rusqlite::Error>(entry)
    })
    .await;

    match result {
        Ok(Ok(Some(entry))) => Ok((axum::http::StatusCode::OK, Json(entry)).into_response()),
        Ok(Ok(None)) => Err((axum::http::StatusCode::NOT_FOUND, "Entry not found").into_response()),
        Ok(Err(e)) => {
            event!(Level::ERROR, "error in get_entry: {:?}", e);
            Err((
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error",
            )
                .into_response())
        }
        Err(e) => {
            event!(Level::ERROR, "task error in get_entry: {:?}", e);
            Err((
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error",
            )
                .into_response())
        }
    }
}
