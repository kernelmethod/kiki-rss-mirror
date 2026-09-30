use super::create_tag::reserved_name_response;
use crate::db::tags::{is_reserved_tag_name, TagKind};
use crate::server::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{event, Level};

#[derive(Deserialize, Serialize, utoipa::ToSchema)]
pub struct UpdateTagRequest {
    pub name: String,
}

#[derive(Deserialize, Serialize, utoipa::ToSchema)]
pub struct UpdateTagResponse {
    pub id: i64,
    pub name: String,
    /// Always `user`, since system tags cannot be renamed.
    pub kind: TagKind,
}

#[derive(Error, Debug)]
enum UpdateTagTaskError {
    #[error("tag not found")]
    TagNotFound,

    #[error("system tags cannot be renamed")]
    SystemTag,

    #[error("tag name already exists")]
    AlreadyExists,

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
}

/// Update a tag
///
/// Rename the user tag with the provided ID. System tags cannot be renamed, and the new name may
/// not start with the `system:` prefix.
#[utoipa::path(
    put,
    path = "/v1/tags/id/{id}",
    params(
        ("id" = i64, Path, description = "Tag ID"),
    ),
    request_body = UpdateTagRequest,
    responses(
        (status = 200, description = "Tag updated successfully", body = UpdateTagResponse),
        (status = 400, description = "Tag name is reserved for system tags"),
        (status = 403, description = "System tags cannot be renamed"),
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
    if is_reserved_tag_name(&payload.name) {
        return Err(reserved_name_response());
    }

    let result = state
        .db
        .write(move |conn| {
            // Check that the tag exists and is a user tag
            let kind = conn
                .prepare("SELECT kind FROM tags WHERE id = ?1")
                .inspect_err(|e| {
                    event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
                })?
                .query_row([id], |row| row.get::<_, TagKind>(0));

            match kind {
                Ok(TagKind::User) => {}
                Ok(TagKind::System) => return Err(UpdateTagTaskError::SystemTag),
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    return Err(UpdateTagTaskError::TagNotFound)
                }
                Err(e) => return Err(UpdateTagTaskError::Database(e)),
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
                kind: TagKind::User,
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
        Ok(Err(UpdateTagTaskError::SystemTag)) => {
            Err((StatusCode::FORBIDDEN, "System tags cannot be renamed").into_response())
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
