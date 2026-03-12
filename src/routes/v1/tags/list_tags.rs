use crate::server::AppState;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use tokio::task;
use tracing::{event, Level};

const DEFAULT_LIMIT: usize = 50;

#[derive(Deserialize, utoipa::IntoParams)]
pub struct ListTagsQueryParams {
    /// Number of records to skip (default: 0).
    pub offset: Option<usize>,
    /// Maximum number of records to return (default: 50).
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
pub struct TagResponse {
    pub id: i64,
    pub name: String,
}

#[derive(Deserialize, Serialize, utoipa::ToSchema)]
pub struct ListTagsResponse {
    pub tags: Vec<TagResponse>,
    pub count: usize,
    pub offset: usize,
    pub limit: usize,
}

/// Route handler for listing all tags.
#[utoipa::path(
    get,
    path = "/v1/tags",
    params(ListTagsQueryParams),
    responses(
        (status = 200, description = "List of tags", body = ListTagsResponse),
        (status = 500, description = "Internal server error"),
    ),
    tag = "tags"
)]
#[axum::debug_handler]
pub async fn list_tags(
    State(state): State<AppState>,
    Query(params): Query<ListTagsQueryParams>,
) -> Result<Response, Response> {
    let conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;
    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT);

    let result = task::spawn_blocking(move || {
        let count = conn
            .prepare("SELECT COUNT(*) FROM tags")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row([], |count| count.get(0))?;

        let tags = conn
            .prepare("SELECT id, name FROM tags LIMIT ?1 OFFSET ?2")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_map([limit, offset], |row| {
                Ok(TagResponse {
                    id: row.get(0)?,
                    name: row.get(1)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok::<ListTagsResponse, rusqlite::Error>(ListTagsResponse {
            count,
            offset,
            limit,
            tags,
        })
    })
    .await;

    match result {
        Ok(Ok(response)) => Ok(Json(response).into_response()),
        Ok(Err(e)) => {
            event!(Level::ERROR, "error in list_tags: {:?}", e);
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(e) => {
            event!(Level::ERROR, "task error in list_tags: {:?}", e);
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
    }
}
