use crate::server::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use rusqlite::TransactionBehavior;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::task;
use tracing::{event, Level};

use crate::routes::v1::tags::list_tags::TagResponse;

#[derive(Deserialize, Serialize, utoipa::ToSchema)]
#[cfg_attr(feature = "mcp", derive(schemars::JsonSchema))]
pub struct SetFeedTagsRequest {
    pub tag_ids: Vec<i64>,
}

#[derive(Deserialize, Serialize, utoipa::ToSchema)]
pub struct GetFeedTagsResponse {
    pub tags: Vec<TagResponse>,
}

#[derive(Error, Debug)]
enum FeedTagsTaskError {
    #[error("feed not found")]
    FeedNotFound,

    #[error("tag not found: {0}")]
    TagNotFound(i64),

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
}

/// Get feed tags
///
/// Get all tags that are associated with a feed. These tags are automatically applied to any
/// entries that are retrieved by that feed.
#[utoipa::path(
    get,
    path = "/v1/feeds/id/{id}/tags",
    params(
        ("id" = i64, Path, description = "Feed ID"),
    ),
    responses(
        (status = 200, description = "Tags for the feed", body = GetFeedTagsResponse),
        (status = 404, description = "Feed not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "feeds"
)]
#[axum::debug_handler]
pub async fn get_feed_tags(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, Response> {
    let conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;

    let result = task::spawn_blocking(move || {
        // Check if feed exists
        let exists: bool = conn
            .prepare("SELECT EXISTS(SELECT 1 FROM feeds WHERE id = ?1)")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row([id], |row| row.get(0))?;

        if !exists {
            return Err(FeedTagsTaskError::FeedNotFound);
        }

        let tags = conn
            .prepare(
                "SELECT t.id, t.name FROM tags t
                 INNER JOIN feed_tags ft ON ft.tag_id = t.id
                 WHERE ft.feed_id = ?1",
            )
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_map([id], |row| {
                Ok(TagResponse {
                    id: row.get(0)?,
                    name: row.get(1)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok::<GetFeedTagsResponse, FeedTagsTaskError>(GetFeedTagsResponse { tags })
    })
    .await
    .inspect_err(|e| {
        event!(Level::ERROR, "task error in get_feed_tags: {:?}", e);
    });

    match result {
        Ok(Ok(response)) => Ok(Json(response).into_response()),
        Ok(Err(FeedTagsTaskError::FeedNotFound)) => {
            Err((StatusCode::NOT_FOUND, "Feed not found").into_response())
        }
        Ok(Err(_)) => {
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(_) => Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()),
    }
}

/// Set feed tags
///
/// Set the list of tags that are applied to a feed. This endpoint replaces all tags that are
/// currently applied to that feed.
///
/// Tags applied at a feed level are automatically applied to all entries retrieved from that feed.
#[utoipa::path(
    put,
    path = "/v1/feeds/id/{id}/tags",
    params(
        ("id" = i64, Path, description = "Feed ID"),
    ),
    request_body = SetFeedTagsRequest,
    responses(
        (status = 200, description = "Tags updated for the feed", body = GetFeedTagsResponse),
        (status = 400, description = "One or more tag IDs not found"),
        (status = 404, description = "Feed not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "feeds"
)]
#[axum::debug_handler]
pub async fn set_feed_tags(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(payload): Json<SetFeedTagsRequest>,
) -> Result<Response, Response> {
    let mut conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;

    let result = task::spawn_blocking(move || {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to begin transaction: {:?}", e);
            })?;

        // Check if feed exists
        let exists: bool = tx
            .prepare("SELECT EXISTS(SELECT 1 FROM feeds WHERE id = ?1)")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row([id], |row| row.get(0))?;

        if !exists {
            return Err(FeedTagsTaskError::FeedNotFound);
        }

        // Validate all tag IDs exist
        for &tag_id in &payload.tag_ids {
            let tag_exists: bool = tx
                .prepare("SELECT EXISTS(SELECT 1 FROM tags WHERE id = ?1)")
                .inspect_err(|e| {
                    event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
                })?
                .query_row([tag_id], |row| row.get(0))?;

            if !tag_exists {
                return Err(FeedTagsTaskError::TagNotFound(tag_id));
            }
        }

        // Delete existing associations
        tx.prepare("DELETE FROM feed_tags WHERE feed_id = ?1")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .execute([id])?;

        // Insert new associations
        {
            let mut stmt = tx
                .prepare("INSERT INTO feed_tags (feed_id, tag_id) VALUES (?1, ?2)")
                .inspect_err(|e| {
                    event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
                })?;

            for &tag_id in &payload.tag_ids {
                stmt.execute(rusqlite::params![id, tag_id])?;
            }
        }

        // Return the updated tags
        let tags = tx
            .prepare(
                "SELECT t.id, t.name FROM tags t
                 INNER JOIN feed_tags ft ON ft.tag_id = t.id
                 WHERE ft.feed_id = ?1",
            )
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_map([id], |row| {
                Ok(TagResponse {
                    id: row.get(0)?,
                    name: row.get(1)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        tx.commit().inspect_err(|e| {
            event!(Level::ERROR, "unable to commit transaction: {:?}", e);
        })?;

        Ok::<GetFeedTagsResponse, FeedTagsTaskError>(GetFeedTagsResponse { tags })
    })
    .await
    .inspect_err(|e| {
        event!(Level::ERROR, "task error in set_feed_tags: {:?}", e);
    });

    match result {
        Ok(Ok(response)) => Ok(Json(response).into_response()),
        Ok(Err(FeedTagsTaskError::FeedNotFound)) => {
            Err((StatusCode::NOT_FOUND, "Feed not found").into_response())
        }
        Ok(Err(FeedTagsTaskError::TagNotFound(tag_id))) => Err((
            StatusCode::BAD_REQUEST,
            format!("Tag not found: {}", tag_id),
        )
            .into_response()),
        Ok(Err(_)) => {
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(_) => Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()),
    }
}
