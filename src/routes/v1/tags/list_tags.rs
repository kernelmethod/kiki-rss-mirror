use crate::db::tags::TagKind;
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
    /// Only return tags of this kind (default: all tags).
    pub kind: Option<TagKind>,
}

#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
pub struct TagResponse {
    pub id: i64,
    pub name: String,
    /// Whether this is a user tag or a system tag.
    pub kind: TagKind,
}

impl TagResponse {
    /// Build a response from a row whose first three columns are the tag's
    /// `id`, `name`, and `kind`.
    pub fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(TagResponse {
            id: row.get(0)?,
            name: row.get(1)?,
            kind: row.get(2)?,
        })
    }
}

#[derive(Deserialize, Serialize, utoipa::ToSchema)]
pub struct ListTagsResponse {
    pub tags: Vec<TagResponse>,
    pub count: usize,
    pub offset: usize,
    pub limit: usize,
}

/// List all tags
///
/// Retrieve a paginated list of all tags that are known to the server, optionally
/// restricted to user tags or system tags.
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
    let kind = params.kind.map(TagKind::as_str);

    let result = task::spawn_blocking(move || {
        let count = conn
            .prepare("SELECT COUNT(*) FROM tags WHERE ?1 IS NULL OR kind = ?1")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row([kind], |count| count.get(0))?;

        let tags = conn
            .prepare(
                "SELECT id, name, kind FROM tags WHERE ?1 IS NULL OR kind = ?1
                 ORDER BY id LIMIT ?2 OFFSET ?3",
            )
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_map(
                rusqlite::params![kind, limit, offset],
                TagResponse::from_row,
            )?
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
