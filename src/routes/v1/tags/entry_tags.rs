use crate::server::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::task;
use tracing::{event, Level};

use super::list_tags::TagResponse;

#[derive(Deserialize, Serialize)]
pub struct SetEntryTagsRequest {
    pub tag_ids: Vec<i64>,
}

#[derive(Deserialize, Serialize)]
pub struct GetEntryTagsResponse {
    pub tags: Vec<TagResponse>,
}

#[derive(Error, Debug)]
enum EntryTagsTaskError {
    #[error("entry not found")]
    EntryNotFound,

    #[error("tag not found: {0}")]
    TagNotFound(i64),

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
}

/// Route handler for getting tags associated with an entry.
#[axum::debug_handler]
pub async fn get_entry_tags(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, Response> {
    let conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;

    let result = task::spawn_blocking(move || {
        // Check if entry exists
        let exists: bool = conn
            .prepare("SELECT EXISTS(SELECT 1 FROM entries WHERE id = ?1)")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row([id], |row| row.get(0))?;

        if !exists {
            return Err(EntryTagsTaskError::EntryNotFound);
        }

        let tags = conn
            .prepare(
                "SELECT t.id, t.name FROM tags t
                 INNER JOIN entry_tags et ON et.tag_id = t.id
                 WHERE et.entry_id = ?1",
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

        Ok::<GetEntryTagsResponse, EntryTagsTaskError>(GetEntryTagsResponse { tags })
    })
    .await
    .inspect_err(|e| {
        event!(Level::ERROR, "task error in get_entry_tags: {:?}", e);
    });

    match result {
        Ok(Ok(response)) => Ok(Json(response).into_response()),
        Ok(Err(EntryTagsTaskError::EntryNotFound)) => {
            Err((StatusCode::NOT_FOUND, "Entry not found").into_response())
        }
        Ok(Err(_)) => {
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(_) => Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()),
    }
}

/// Route handler for setting tags on an entry (replaces existing).
#[axum::debug_handler]
pub async fn set_entry_tags(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(payload): Json<SetEntryTagsRequest>,
) -> Result<Response, Response> {
    let conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;

    let result = task::spawn_blocking(move || {
        // Check if entry exists
        let exists: bool = conn
            .prepare("SELECT EXISTS(SELECT 1 FROM entries WHERE id = ?1)")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row([id], |row| row.get(0))?;

        if !exists {
            return Err(EntryTagsTaskError::EntryNotFound);
        }

        // Validate all tag IDs exist
        for &tag_id in &payload.tag_ids {
            let tag_exists: bool = conn
                .prepare("SELECT EXISTS(SELECT 1 FROM tags WHERE id = ?1)")
                .inspect_err(|e| {
                    event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
                })?
                .query_row([tag_id], |row| row.get(0))?;

            if !tag_exists {
                return Err(EntryTagsTaskError::TagNotFound(tag_id));
            }
        }

        // Delete existing associations
        conn.prepare("DELETE FROM entry_tags WHERE entry_id = ?1")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .execute([id])?;

        // Insert new associations
        let mut stmt = conn
            .prepare("INSERT INTO entry_tags (entry_id, tag_id) VALUES (?1, ?2)")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?;

        for &tag_id in &payload.tag_ids {
            stmt.execute(rusqlite::params![id, tag_id])?;
        }

        // Return the updated tags
        let tags = conn
            .prepare(
                "SELECT t.id, t.name FROM tags t
                 INNER JOIN entry_tags et ON et.tag_id = t.id
                 WHERE et.entry_id = ?1",
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

        Ok::<GetEntryTagsResponse, EntryTagsTaskError>(GetEntryTagsResponse { tags })
    })
    .await
    .inspect_err(|e| {
        event!(Level::ERROR, "task error in set_entry_tags: {:?}", e);
    });

    match result {
        Ok(Ok(response)) => Ok(Json(response).into_response()),
        Ok(Err(EntryTagsTaskError::EntryNotFound)) => {
            Err((StatusCode::NOT_FOUND, "Entry not found").into_response())
        }
        Ok(Err(EntryTagsTaskError::TagNotFound(tag_id))) => Err((
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
