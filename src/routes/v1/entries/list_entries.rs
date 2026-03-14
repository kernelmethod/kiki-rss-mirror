use crate::server::AppState;
use axum::{
    extract::{Query, State},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use tokio::task;
use tracing::{event, Level};

pub const DEFAULT_LIMIT: usize = 50;

#[derive(Deserialize, utoipa::IntoParams)]
pub struct ListEntriesQueryParams {
    /// Number of records to skip (default: 0).
    pub offset: Option<usize>,
    /// Maximum number of records to return (default: 50).
    pub limit: Option<usize>,
}

#[derive(Deserialize, Serialize)]
pub struct ListEntriesError {
    pub message: String,
}

#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
pub struct ListEntriesResponseEntry {
    pub id: i64,
    pub feed_id: Option<i64>,
    pub source_id: Option<i64>,
    /// Syndication format: "rss" or "atom".
    pub syndication_format: String,
    pub guid: String,
    /// Publication time in RFC3339 format.
    pub published_at: Option<String>,
    pub title: String,
    pub url: String,
    pub content: Option<String>,
}

#[derive(Deserialize, Serialize, utoipa::ToSchema)]
pub struct ListEntriesResponse {
    pub entries: Vec<ListEntriesResponseEntry>,
    pub count: usize,
    pub offset: usize,
    pub limit: usize,
}

impl Default for ListEntriesError {
    fn default() -> Self {
        ListEntriesError {
            message: "internal error".to_string(),
        }
    }
}

/// List all entries
///
/// Retrieve a paginated list of all RSS and Atom entries that the server has retrieved.
#[utoipa::path(
    get,
    path = "/v1/entries",
    params(ListEntriesQueryParams),
    responses(
        (status = 200, description = "List of feed entries", body = ListEntriesResponse),
        (status = 500, description = "Internal server error"),
    ),
    tag = "entries"
)]
pub async fn list_entries(
    State(state): State<AppState>,
    Query(params): Query<ListEntriesQueryParams>,
) -> Response {
    let conn = match state.conn_pool.get() {
        Ok(conn) => conn,
        Err(e) => {
            event!(Level::ERROR, "failed to get database connection: {:?}", e);
            let error = ListEntriesError::default();
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, Json(error)).into_response();
        }
    };
    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT);

    let result = task::spawn_blocking(move || {
        let count = conn
            .prepare("SELECT COUNT(*) FROM entries")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row([], |count| count.get(0))?;

        let entries = conn
            .prepare(
                "SELECT id, feed_id, source_id, syndication_format,
                    guid, published_at, title, url, content
                FROM entries LIMIT ?1 OFFSET ?2",
            )
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_map([limit, offset], |row| {
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
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .inspect_err(|e| {
                event!(Level::ERROR, "failed to create entry list: {:?}", e);
            })?;

        Ok::<ListEntriesResponse, rusqlite::Error>(ListEntriesResponse {
            count,
            offset,
            limit,
            entries,
        })
    })
    .await;

    match result {
        Ok(Ok(response)) => Json(response).into_response(),
        Ok(Err(e)) => {
            event!(Level::ERROR, "error in list_entries: {:?}", e);
            let error = ListEntriesError::default();
            (axum::http::StatusCode::INTERNAL_SERVER_ERROR, Json(error)).into_response()
        }
        Err(e) => {
            event!(Level::ERROR, "task error in list_entries: {:?}", e);
            let error = ListEntriesError::default();
            (axum::http::StatusCode::INTERNAL_SERVER_ERROR, Json(error)).into_response()
        }
    }
}
