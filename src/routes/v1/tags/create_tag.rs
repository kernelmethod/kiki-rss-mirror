use crate::server::AppState;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::task;
use tracing::{event, Level};

#[derive(Deserialize, Serialize)]
pub struct CreateTagRequest {
    pub name: String,
}

#[derive(Deserialize, Serialize)]
pub struct CreateTagResponse {
    pub id: i64,
    pub name: String,
}

#[derive(Error, Debug)]
enum CreateTagTaskError {
    #[error("tag already exists")]
    AlreadyExists,

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
}

/// Route handler for creating a new tag.
#[axum::debug_handler]
pub async fn create_tag(
    State(state): State<AppState>,
    Json(payload): Json<CreateTagRequest>,
) -> Result<Response, Response> {
    let conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;

    let result = task::spawn_blocking(move || {
        let result = conn
            .prepare("INSERT INTO tags (name) VALUES (?1) RETURNING id, name")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row([&payload.name], |row| {
                Ok(CreateTagResponse {
                    id: row.get(0)?,
                    name: row.get(1)?,
                })
            });

        match result {
            Ok(tag) => Ok(tag),
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Err(CreateTagTaskError::AlreadyExists)
            }
            Err(e) => Err(CreateTagTaskError::Database(e)),
        }
    })
    .await
    .inspect_err(|e| {
        event!(Level::ERROR, "task error in create_tag: {:?}", e);
    });

    match result {
        Ok(Ok(tag)) => Ok((StatusCode::CREATED, Json(tag)).into_response()),
        Ok(Err(CreateTagTaskError::AlreadyExists)) => {
            Err((StatusCode::CONFLICT, "Tag already exists").into_response())
        }
        Ok(Err(CreateTagTaskError::Database(_))) => {
            event!(Level::ERROR, "database error in create_tag");
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(_) => Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()),
    }
}
