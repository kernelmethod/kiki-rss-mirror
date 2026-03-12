use crate::routes::v1::entries::ListEntriesResponseEntry;
use crate::server::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::task;
use tracing::{event, Level};

const DEFAULT_LIMIT: usize = 50;

#[derive(Deserialize)]
pub struct TagEntriesQueryParams {
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

#[derive(Deserialize, Serialize)]
pub struct TagEntriesResponse {
    pub entries: Vec<ListEntriesResponseEntry>,
    pub count: usize,
    pub offset: usize,
    pub limit: usize,
}

#[derive(Error, Debug)]
enum TagEntriesTaskError {
    #[error("tag not found")]
    TagNotFound,

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
}

/// Route handler for listing entries associated with a tag.
#[axum::debug_handler]
pub async fn tag_entries(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(params): Query<TagEntriesQueryParams>,
) -> Result<Response, Response> {
    let conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;
    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT);

    let result = task::spawn_blocking(move || {
        // Check if tag exists
        let exists: bool = conn
            .prepare("SELECT EXISTS(SELECT 1 FROM tags WHERE id = ?1)")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row([id], |row| row.get(0))?;

        if !exists {
            return Err(TagEntriesTaskError::TagNotFound);
        }

        let count: usize = conn
            .prepare(
                "SELECT COUNT(*) FROM entries e
                 INNER JOIN entry_tags et ON et.entry_id = e.id
                 WHERE et.tag_id = ?1",
            )
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row([id], |row| row.get(0))?;

        let entries = conn
            .prepare(
                "SELECT e.id, e.feed_id, e.source_id, e.syndication_format,
                        e.guid, e.published_at, e.title, e.url, e.content,
                        e.status_read, e.status_favorite
                 FROM entries e
                 INNER JOIN entry_tags et ON et.entry_id = e.id
                 WHERE et.tag_id = ?1
                 LIMIT ?2 OFFSET ?3",
            )
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_map(rusqlite::params![id, limit, offset], |row| {
                Ok(ListEntriesResponseEntry {
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
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok::<TagEntriesResponse, TagEntriesTaskError>(TagEntriesResponse {
            entries,
            count,
            offset,
            limit,
        })
    })
    .await
    .inspect_err(|e| {
        event!(Level::ERROR, "task error in tag_entries: {:?}", e);
    });

    match result {
        Ok(Ok(response)) => Ok(Json(response).into_response()),
        Ok(Err(TagEntriesTaskError::TagNotFound)) => {
            Err((StatusCode::NOT_FOUND, "Tag not found").into_response())
        }
        Ok(Err(_)) => {
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(_) => Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()),
    }
}
