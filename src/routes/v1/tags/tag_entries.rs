use crate::auth::{self, Principal, Scope};
use crate::routes::v1::entries::rows::{
    attach_tags, entry_columns, entry_from_row, id_range, EntrySort,
};
use crate::routes::v1::entries::ListEntriesResponseEntry;
use crate::server::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{event, Level};

const DEFAULT_LIMIT: usize = 50;

#[derive(Deserialize, utoipa::IntoParams)]
pub struct TagEntriesQueryParams {
    /// Number of records to skip (default: 0).
    pub offset: Option<usize>,
    /// Maximum number of records to return (default: 50).
    pub limit: Option<usize>,
    /// Only list entries with an ID greater than this. With `sort=id`, pass
    /// the last ID of one page to get the next.
    pub since_id: Option<i64>,
    /// Only list entries with an ID less than this. With `sort=id_desc`,
    /// pass the last ID of one page to get the next.
    pub max_id: Option<i64>,
    /// The order to list entries in (default: `published_at`).
    pub sort: Option<EntrySort>,
}

#[derive(Deserialize, Serialize, utoipa::ToSchema)]
pub struct TagEntriesResponse {
    pub entries: Vec<ListEntriesResponseEntry>,
    pub count: usize,
    pub offset: usize,
    pub limit: usize,
}

#[derive(Error, Debug)]
enum TagEntriesTaskError {
    #[error("tag not found")]
    TagNotFound,

    #[error("only tokens with the tags scope may tag entries with user tags")]
    NeedsTagsScope,

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
}

/// Get entries by tag
///
/// Retrieve a paginated list of entries associated with a tag, newest first, or in
/// the order given by `sort`. Entries with the same publication time are ordered by
/// descending ID.
#[utoipa::path(
    get,
    path = "/v1/tags/id/{id}/entries",
    params(
        ("id" = i64, Path, description = "Tag ID"),
        TagEntriesQueryParams,
    ),
    responses(
        (status = 200, description = "Entries associated with the tag", body = TagEntriesResponse),
        (status = 404, description = "Tag not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "tags"
)]
#[axum::debug_handler]
pub async fn tag_entries(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(params): Query<TagEntriesQueryParams>,
) -> Result<Response, Response> {
    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
    let (since_id, max_id) = (params.since_id, params.max_id);
    let sort = params.sort.unwrap_or_default();

    let result = state
        .db
        .read(move |conn| {
            // Check if tag exists
            let exists: bool = conn
                .prepare("SELECT EXISTS(SELECT 1 FROM tags WHERE id = ?1)")
                .inspect_err(|e| {
                    event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
                })?
                .query_row([id], |row| row.get(0))?;

            if !exists {
                return Err(TagEntriesTaskError::TagNotFound);
            }

            let count: usize = conn
                .prepare(&format!(
                    "SELECT COUNT(*) FROM entries e
                 INNER JOIN entry_tags et ON et.entry_id = e.id
                 WHERE et.tag_id = ?1 AND {}",
                    id_range(since_id, max_id)
                ))
                .inspect_err(|e| {
                    event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
                })?
                .query_row([id], |row| row.get(0))?;

            let mut entries = conn
                .prepare(&format!(
                    "SELECT {}
                 FROM entries e
                 INNER JOIN entry_tags et ON et.entry_id = e.id
                 WHERE et.tag_id = ?1 AND {}
                 ORDER BY {}
                 LIMIT ?2 OFFSET ?3",
                    entry_columns(),
                    id_range(since_id, max_id),
                    sort.order_by()
                ))
                .inspect_err(|e| {
                    event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
                })?
                .query_map(rusqlite::params![id, limit, offset], entry_from_row)?
                .collect::<Result<Vec<_>, _>>()?;
            attach_tags(conn, &mut entries)?;

            Ok::<TagEntriesResponse, TagEntriesTaskError>(TagEntriesResponse {
                entries,
                count,
                offset,
                limit,
            })
        })
        .await
        .inspect_err(|e| {
            event!(Level::ERROR, "task error in tag_entries: {:?}", e);
        });

    match result {
        Ok(Ok(response)) => Ok(Json(response).into_response()),
        Ok(Err(TagEntriesTaskError::TagNotFound)) => {
            Err((StatusCode::NOT_FOUND, "Tag not found").into_response())
        }
        Ok(Err(_)) => {
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(_) => Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()),
    }
}

/// Check that tag `id` exists, and that entries may be added to or removed
/// from it: any tag if `may_tag`, the request holding the `tags` scope, and
/// otherwise only system tags, which record entries' state.
fn check_tag(
    conn: &rusqlite::Connection,
    id: i64,
    may_tag: bool,
) -> Result<(), TagEntriesTaskError> {
    let kind: Option<String> = conn
        .query_row("SELECT kind FROM tags WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .optional()?;
    match kind.as_deref() {
        None => Err(TagEntriesTaskError::TagNotFound),
        Some("system") => Ok(()),
        Some(_) if may_tag => Ok(()),
        Some(_) => Err(TagEntriesTaskError::NeedsTagsScope),
    }
}

#[derive(Debug, Default, Deserialize, Serialize, utoipa::ToSchema)]
pub struct AddTagEntriesRequest {
    /// Only tag entries with an ID no greater than this, e.g. the newest entry the user has
    /// seen, so that entries fetched since are left alone.
    #[serde(default)]
    pub up_to_id: Option<i64>,
    /// Only tag entries from this feed.
    #[serde(default)]
    pub feed_id: Option<i64>,
    /// Only tag the entries with these IDs. IDs of entries that do not exist
    /// are ignored.
    #[serde(default)]
    pub entry_ids: Option<Vec<i64>>,
}

impl AddTagEntriesRequest {
    /// An SQL condition on the `entries` table that holds for the entries
    /// this request selects, using query parameters 2, 3 and 4 (bound by
    /// [`AddTagEntriesRequest::sql_params`]).
    const CONDITION: &'static str = "(?2 IS NULL OR id <= ?2) AND (?3 IS NULL OR feed_id = ?3)
                 AND (?4 IS NULL OR id IN (SELECT value FROM json_each(?4)))";

    /// The values of query parameters 2, 3 and 4 in
    /// [`AddTagEntriesRequest::CONDITION`].
    fn sql_params(&self) -> (Option<i64>, Option<i64>, Option<String>) {
        let entry_ids = self.entry_ids.as_ref().map(|ids| {
            // A list of integers always serializes
            serde_json::to_string(ids).unwrap_or_else(|_| "[]".to_string())
        });
        (self.up_to_id, self.feed_id, entry_ids)
    }
}

#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
pub struct AddTagEntriesResponse {
    /// Number of entries that did not have the tag before, and now do.
    pub tagged: usize,
}

/// Add a tag to many entries
///
/// Apply a tag to every entry matching the request, in one go. Works for both user tags and
/// system tags, e.g. `system:read` to mark entries as read. An empty request body (`{}`) tags
/// every entry; `up_to_id`, `feed_id` and `entry_ids` narrow it down, and all of those given
/// must hold. Entries that already have the tag are left as they are.
#[utoipa::path(
    post,
    path = "/v1/tags/id/{id}/entries",
    params(
        ("id" = i64, Path, description = "Tag ID"),
    ),
    request_body = AddTagEntriesRequest,
    responses(
        (status = 200, description = "Entries tagged", body = AddTagEntriesResponse),
        (status = 403, description = "The token may only add entries to system tags"),
        (status = 404, description = "Tag not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "tags"
)]
#[axum::debug_handler]
pub async fn add_tag_entries(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<i64>,
    Json(request): Json<AddTagEntriesRequest>,
) -> Result<Response, Response> {
    let may_tag = principal.allows(Scope::Tags);
    let result = state
        .db
        .write(move |conn| {
            check_tag(conn, id, may_tag)?;

            let (up_to_id, feed_id, entry_ids) = request.sql_params();
            let tagged = conn
                .prepare(&format!(
                    "INSERT OR IGNORE INTO entry_tags (entry_id, tag_id)
                 SELECT id, ?1 FROM entries
                 WHERE {}",
                    AddTagEntriesRequest::CONDITION
                ))
                .inspect_err(|e| {
                    event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
                })?
                .execute(rusqlite::params![id, up_to_id, feed_id, entry_ids])?;

            Ok::<AddTagEntriesResponse, TagEntriesTaskError>(AddTagEntriesResponse { tagged })
        })
        .await
        .inspect_err(|e| {
            event!(Level::ERROR, "task error in add_tag_entries: {:?}", e);
        });

    match result {
        Ok(Ok(response)) => Ok(Json(response).into_response()),
        Ok(Err(TagEntriesTaskError::TagNotFound)) => {
            Err((StatusCode::NOT_FOUND, "Tag not found").into_response())
        }
        Ok(Err(TagEntriesTaskError::NeedsTagsScope)) => Err(auth::forbidden(Scope::Tags)),
        Ok(Err(e)) => {
            event!(Level::ERROR, "error in add_tag_entries: {:?}", e);
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(_) => Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()),
    }
}

#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
pub struct RemoveTagEntriesResponse {
    /// Number of entries that had the tag before, and now do not.
    pub untagged: usize,
}

/// Remove a tag from many entries
///
/// Remove a tag from every entry matching the request, in one go: the counterpart of adding a
/// tag to many entries, taking the same request body. Works for both user tags and system
/// tags, e.g. `system:read` to mark entries as unread. An empty request body (`{}`) removes the
/// tag from every entry; `up_to_id`, `feed_id` and `entry_ids` narrow it down, and all of those
/// given must hold.
#[utoipa::path(
    delete,
    path = "/v1/tags/id/{id}/entries",
    params(
        ("id" = i64, Path, description = "Tag ID"),
    ),
    request_body = AddTagEntriesRequest,
    responses(
        (status = 200, description = "Tag removed from entries", body = RemoveTagEntriesResponse),
        (status = 403, description = "The token may only add entries to system tags"),
        (status = 404, description = "Tag not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "tags"
)]
#[axum::debug_handler]
pub async fn remove_tag_entries(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<i64>,
    Json(request): Json<AddTagEntriesRequest>,
) -> Result<Response, Response> {
    let may_tag = principal.allows(Scope::Tags);
    let result = state
        .db
        .write(move |conn| {
            check_tag(conn, id, may_tag)?;

            let (up_to_id, feed_id, entry_ids) = request.sql_params();
            let untagged = conn
                .prepare(&format!(
                    "DELETE FROM entry_tags
                 WHERE tag_id = ?1 AND entry_id IN (SELECT id FROM entries WHERE {})",
                    AddTagEntriesRequest::CONDITION
                ))
                .inspect_err(|e| {
                    event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
                })?
                .execute(rusqlite::params![id, up_to_id, feed_id, entry_ids])?;

            Ok::<RemoveTagEntriesResponse, TagEntriesTaskError>(RemoveTagEntriesResponse {
                untagged,
            })
        })
        .await
        .inspect_err(|e| {
            event!(Level::ERROR, "task error in remove_tag_entries: {:?}", e);
        });

    match result {
        Ok(Ok(response)) => Ok(Json(response).into_response()),
        Ok(Err(TagEntriesTaskError::TagNotFound)) => {
            Err((StatusCode::NOT_FOUND, "Tag not found").into_response())
        }
        Ok(Err(TagEntriesTaskError::NeedsTagsScope)) => Err(auth::forbidden(Scope::Tags)),
        Ok(Err(e)) => {
            event!(Level::ERROR, "error in remove_tag_entries: {:?}", e);
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(_) => Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()),
    }
}
