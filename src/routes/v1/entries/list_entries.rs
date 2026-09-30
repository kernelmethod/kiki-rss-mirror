use crate::routes::v1::entries::rows::{
    attach_tags, entry_columns, entry_from_row, id_range, EntrySort,
};
use crate::routes::v1::tags::list_tags::TagResponse;
use crate::server::AppState;
use axum::{
    extract::{Query, State},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
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
    /// Only list entries with an ID greater than this. With `sort=id`, pass
    /// the last ID of one page to get the next.
    pub since_id: Option<i64>,
    /// Only list entries with an ID less than this. With `sort=id_desc`,
    /// pass the last ID of one page to get the next.
    pub max_id: Option<i64>,
    /// The order to list entries in (default: `published_at`).
    pub sort: Option<EntrySort>,
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
    /// When Kiki first stored the entry, in RFC3339 format.
    #[serde(default)]
    pub ingested_at: Option<String>,
    /// Relative Kiki URL that serves the favicon of the website the
    /// entry's feed belongs to, or `null` if it has not been cached.
    #[serde(default)]
    pub feed_favicon_url: Option<String>,
    /// Every tag applied to the entry, both user tags and system tags (such
    /// as `system:read` and `system:saved`).
    #[serde(default)]
    pub tags: Vec<TagResponse>,
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
/// newest first, or in the order given by `sort`. Entries with the same publication time
/// are ordered by descending ID. Entries tagged `system:hidden` are left out, and not
/// counted, unless `include_hidden` is true.
///
/// To sync incrementally, list with `sort=id` and `since_id` set to the highest ID seen so
/// far: entry IDs only ever increase, so this returns exactly the entries stored since.
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
    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
    let include_hidden = params.include_hidden.unwrap_or(false);
    let (since_id, max_id) = (params.since_id, params.max_id);
    let sort = params.sort.unwrap_or_default();

    let result = state
        .db
        .read(move |conn| {
            let count = conn
                .prepare(&format!(
                    "SELECT COUNT(*) FROM entries e WHERE {} AND {}",
                    not_hidden_unless(1),
                    id_range(since_id, max_id)
                ))
                .inspect_err(|e| {
                    event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
                })?
                .query_row([include_hidden], |count| count.get(0))?;

            let mut entries = conn
                .prepare(&format!(
                    "SELECT {}
                FROM entries e
                WHERE {} AND {}
                ORDER BY {}
                LIMIT ?1 OFFSET ?2",
                    entry_columns(),
                    not_hidden_unless(3),
                    id_range(since_id, max_id),
                    sort.order_by()
                ))
                .inspect_err(|e| {
                    event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
                })?
                .query_map(
                    rusqlite::params![limit, offset, include_hidden],
                    entry_from_row,
                )?
                .collect::<Result<Vec<_>, _>>()
                .inspect_err(|e| {
                    event!(Level::ERROR, "failed to create entry list: {:?}", e);
                })?;
            attach_tags(conn, &mut entries)?;

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
