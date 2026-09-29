use crate::db::favicons::favicon_hash_sql;
use crate::routes::v1::assets::read_asset_url_column;
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
    /// Also list entries tagged `system:hidden`, which are left out by
    /// default (default: false).
    pub include_hidden: Option<bool>,
}

/// An SQL condition that holds when the entry aliased `e` is not tagged
/// `system:hidden`.
pub(crate) const NOT_HIDDEN: &str = "NOT EXISTS (
            SELECT 1 FROM entry_tags et JOIN tags t ON t.id = et.tag_id
            WHERE et.entry_id = e.id AND t.kind = 'system' AND t.name = 'system:hidden'
        )";

/// An SQL condition that holds when the entry aliased `e` is neither tagged
/// `system:hidden` nor `system:read`.
pub(crate) const UNREAD: &str = "NOT EXISTS (
            SELECT 1 FROM entry_tags et JOIN tags t ON t.id = et.tag_id
            WHERE et.entry_id = e.id AND t.kind = 'system'
              AND t.name IN ('system:hidden', 'system:read')
        )";

/// An SQL condition that holds when the entry aliased `e` is not tagged
/// `system:hidden`, or when query parameter number `param` is true.
pub(crate) fn not_hidden_unless(param: usize) -> String {
    format!("(?{param} OR {NOT_HIDDEN})")
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
    /// Relative Kiki URL that serves the favicon of the website the
    /// entry's feed belongs to, or `null` if it has not been cached.
    #[serde(default)]
    pub feed_favicon_url: Option<String>,
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
/// Retrieve a paginated list of all RSS and Atom entries that the server has retrieved,
/// newest first. Entries with the same publication time are ordered by descending ID.
/// Entries tagged `system:hidden` are left out, and not counted, unless
/// `include_hidden` is true.
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
    let include_hidden = params.include_hidden.unwrap_or(false);

    let result = task::spawn_blocking(move || {
        let count = conn
            .prepare(&format!(
                "SELECT COUNT(*) FROM entries e WHERE {}",
                not_hidden_unless(1)
            ))
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row([include_hidden], |count| count.get(0))?;

        let entries = conn
            .prepare(&format!(
                "SELECT id, feed_id, source_id, syndication_format,
                    guid, published_at, title, url, content, {}
                FROM entries e
                WHERE {}
                ORDER BY published_at DESC, id DESC
                LIMIT ?1 OFFSET ?2",
                favicon_hash_sql("e.feed_id"),
                not_hidden_unless(3)
            ))
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_map(rusqlite::params![limit, offset, include_hidden], |row| {
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
                    feed_favicon_url: read_asset_url_column(row, 9)?,
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
