use crate::server::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use tokio::task;
use tracing::{event, Level};

#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
pub struct GetTagResponse {
    pub id: i64,
    pub name: String,
}

/// Route handler for getting a single tag by ID.
#[utoipa::path(
    get,
    path = "/v1/tags/id/{id}",
    params(
        ("id" = i64, Path, description = "Tag ID"),
    ),
    responses(
        (status = 200, description = "Tag found", body = GetTagResponse),
        (status = 404, description = "Tag not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "tags"
)]
#[axum::debug_handler]
pub async fn get_tag(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, Response> {
    let conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;

    let result = task::spawn_blocking(move || {
        conn.prepare("SELECT id, name FROM tags WHERE id = ?1")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row([id], |row| {
                Ok(GetTagResponse {
                    id: row.get(0)?,
                    name: row.get(1)?,
                })
            })
    })
    .await
    .inspect_err(|e| {
        event!(Level::ERROR, "task error in get_tag: {:?}", e);
    });

    match result {
        Ok(Ok(tag)) => Ok(Json(tag).into_response()),
        Ok(Err(rusqlite::Error::QueryReturnedNoRows)) => {
            Err((StatusCode::NOT_FOUND, "Tag not found").into_response())
        }
        Ok(Err(_)) => {
            event!(Level::ERROR, "database error in get_tag");
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(_) => Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()),
    }
}
