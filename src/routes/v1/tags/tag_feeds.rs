use crate::routes::v1::feeds::get_feed::GetFeedResponse;
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
pub struct TagFeedsQueryParams {
    /// Number of records to skip (default: 0).
    pub offset: Option<usize>,
    /// Maximum number of records to return (default: 50).
    pub limit: Option<usize>,
}

#[derive(Deserialize, Serialize, utoipa::ToSchema)]
pub struct TagFeedsResponse {
    pub feeds: Vec<GetFeedResponse>,
    pub count: usize,
    pub offset: usize,
    pub limit: usize,
}

#[derive(Error, Debug)]
enum TagFeedsTaskError {
    #[error("tag not found")]
    TagNotFound,

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
}

/// Get feeds by tag
///
/// Retrieve a paginated list of feeds associated with a tag.
#[utoipa::path(
    get,
    path = "/v1/tags/id/{id}/feeds",
    params(
        ("id" = i64, Path, description = "Tag ID"),
        TagFeedsQueryParams,
    ),
    responses(
        (status = 200, description = "Feeds associated with the tag", body = TagFeedsResponse),
        (status = 404, description = "Tag not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "tags"
)]
#[axum::debug_handler]
pub async fn tag_feeds(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(params): Query<TagFeedsQueryParams>,
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
            return Err(TagFeedsTaskError::TagNotFound);
        }

        let count: usize = conn
            .prepare(
                "SELECT COUNT(*) FROM feeds f
                 INNER JOIN feed_tags ft ON ft.feed_id = f.id
                 WHERE ft.tag_id = ?1",
            )
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row([id], |row| row.get(0))?;

        let feeds = conn
            .prepare(
                "SELECT f.id, f.title, f.url, f.description, f.last_checked, f.last_fetch_error, f.last_fetch_error_at, f.min_fetch_interval_seconds
                 FROM feeds f
                 INNER JOIN feed_tags ft ON ft.feed_id = f.id
                 WHERE ft.tag_id = ?1
                 LIMIT ?2 OFFSET ?3",
            )
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_map(rusqlite::params![id, limit, offset], |row| {
                Ok(GetFeedResponse {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    url: row.get(2)?,
                    description: row.get(3)?,
                    last_checked: row.get::<usize, Option<i64>>(4)?.and_then(|ts| {
                        chrono::DateTime::from_timestamp_secs(ts).map(|d| d.to_rfc3339())
                    }),
                    last_fetch_error: row
                        .get::<usize, Option<String>>(5)?
                        .and_then(|s| serde_json::from_str(&s).ok()),
                    last_fetch_error_at: row.get::<usize, Option<i64>>(6)?.and_then(|ts| {
                        chrono::DateTime::from_timestamp_secs(ts).map(|d| d.to_rfc3339())
                    }),
                    min_fetch_interval_seconds: row.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok::<TagFeedsResponse, TagFeedsTaskError>(TagFeedsResponse {
            feeds,
            count,
            offset,
            limit,
        })
    })
    .await
    .inspect_err(|e| {
        event!(Level::ERROR, "task error in tag_feeds: {:?}", e);
    });

    match result {
        Ok(Ok(response)) => Ok(Json(response).into_response()),
        Ok(Err(TagFeedsTaskError::TagNotFound)) => {
            Err((StatusCode::NOT_FOUND, "Tag not found").into_response())
        }
        Ok(Err(_)) => {
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(_) => Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()),
    }
}
