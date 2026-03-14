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

#[derive(Deserialize, Serialize, utoipa::ToSchema)]
pub struct UpdateTagRequest {
    pub name: String,
}

#[derive(Deserialize, Serialize, utoipa::ToSchema)]
pub struct UpdateTagResponse {
    pub id: i64,
    pub name: String,
}

#[derive(Error, Debug)]
enum UpdateTagTaskError {
    #[error("tag not found")]
    TagNotFound,

    #[error("tag name already exists")]
    AlreadyExists,

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
}

/// Update a tag
///
/// Rename the tag with the provided ID.
#[utoipa::path(
    put,
    path = "/v1/tags/id/{id}",
    params(
        ("id" = i64, Path, description = "Tag ID"),
    ),
    request_body = UpdateTagRequest,
    responses(
        (status = 200, description = "Tag updated successfully", body = UpdateTagResponse),
        (status = 404, description = "Tag not found"),
        (status = 409, description = "Tag name already exists"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "tags"
)]
#[axum::debug_handler]
pub async fn update_tag(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(payload): Json<UpdateTagRequest>,
) -> Result<Response, Response> {
    let conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;

    let result = task::spawn_blocking(move || {
        // Check if the tag exists
        let exists: bool = conn
            .prepare("SELECT EXISTS(SELECT 1 FROM tags WHERE id = ?1)")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row([id], |row| row.get(0))?;

        if !exists {
            return Err(UpdateTagTaskError::TagNotFound);
        }

        // Update the tag name
        let update_result = conn
            .prepare("UPDATE tags SET name = ?1 WHERE id = ?2")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .execute(rusqlite::params![&payload.name, id]);

        match update_result {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                return Err(UpdateTagTaskError::AlreadyExists);
            }
            Err(e) => return Err(UpdateTagTaskError::Database(e)),
        }

        Ok(UpdateTagResponse {
            id,
            name: payload.name,
        })
    })
    .await
    .inspect_err(|e| {
        event!(Level::ERROR, "task error in update_tag: {:?}", e);
    });

    match result {
        Ok(Ok(tag)) => Ok(Json(tag).into_response()),
        Ok(Err(UpdateTagTaskError::TagNotFound)) => {
            Err((StatusCode::NOT_FOUND, "Tag not found").into_response())
        }
        Ok(Err(UpdateTagTaskError::AlreadyExists)) => {
            Err((StatusCode::CONFLICT, "Tag name already exists").into_response())
        }
        Ok(Err(UpdateTagTaskError::Database(_))) => {
            event!(Level::ERROR, "database error in update_tag");
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(_) => Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()),
    }
}
