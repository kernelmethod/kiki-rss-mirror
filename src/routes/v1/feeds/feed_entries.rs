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

#[derive(Deserialize, utoipa::IntoParams)]
pub struct FeedEntriesQueryParams {
    /// Number of records to skip (default: 0).
    pub offset: Option<usize>,
    /// Maximum number of records to return (default: 50).
    pub limit: Option<usize>,
}

#[derive(Deserialize, Serialize, utoipa::ToSchema)]
pub struct FeedEntriesResponse {
    pub entries: Vec<ListEntriesResponseEntry>,
    pub count: usize,
    pub offset: usize,
    pub limit: usize,
}

#[derive(Error, Debug)]
enum FeedEntriesTaskError {
    #[error("feed not found")]
    FeedNotFound,

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
}

/// List entries
///
/// List all of the entries belonging to a specific feed.
#[utoipa::path(
    get,
    path = "/v1/feeds/id/{id}/entries",
    params(
        ("id" = i64, Path, description = "Feed ID"),
        FeedEntriesQueryParams,
    ),
    responses(
        (status = 200, description = "List of entries for the feed", body = FeedEntriesResponse),
        (status = 404, description = "Feed not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "feeds"
)]
#[axum::debug_handler]
pub async fn feed_entries(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(params): Query<FeedEntriesQueryParams>,
) -> Result<Response, Response> {
    let conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;
    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT);

    let result = task::spawn_blocking(move || {
        // Check if feed exists
        let exists: bool = conn
            .prepare("SELECT EXISTS(SELECT 1 FROM feeds WHERE id = ?1)")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row([id], |row| row.get(0))?;

        if !exists {
            return Err(FeedEntriesTaskError::FeedNotFound);
        }

        let count: usize = conn
            .prepare("SELECT COUNT(*) FROM entries WHERE feed_id = ?1")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row([id], |row| row.get(0))?;

        let entries = conn
            .prepare(
                "SELECT id, feed_id, source_id, syndication_format,
                        guid, published_at, title, url, content,
                        status_read, status_favorite
                 FROM entries WHERE feed_id = ?1
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

        Ok::<FeedEntriesResponse, FeedEntriesTaskError>(FeedEntriesResponse {
            entries,
            count,
            offset,
            limit,
        })
    })
    .await
    .inspect_err(|e| {
        event!(Level::ERROR, "task error in feed_entries: {:?}", e);
    });

    match result {
        Ok(Ok(response)) => Ok(Json(response).into_response()),
        Ok(Err(FeedEntriesTaskError::FeedNotFound)) => {
            Err((StatusCode::NOT_FOUND, "Feed not found").into_response())
        }
        Ok(Err(_)) => {
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(_) => Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()),
    }
}
