use crate::server::AppState;
use axum::{
    extract::{Path, State},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use tokio::task;
use tracing::{event, Level};

#[derive(Debug, Deserialize, Serialize)]
pub struct GetEntryResponse {
    pub id: i64,
    pub feed_id: i64,
    pub source_id: Option<i64>,
    pub syndication_format: String,
    pub guid: String,
    pub published_at: Option<String>,
    pub title: String,
    pub url: String,
    pub content: Option<String>,
    pub status_read: i32,
    pub status_favorite: i32,
}

pub async fn get_entry(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, Response> {
    let conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;

    let result = task::spawn_blocking(move || {
        let entry = conn
            .prepare(
                "SELECT id, feed_id, source_id, syndication_format,
                    guid, published_at, title, url, content,
                    status_read, status_favorite
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
                    status_read: row.get(9)?,
                    status_favorite: row.get(10)?,
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
