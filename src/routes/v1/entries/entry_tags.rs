use crate::server::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use rusqlite::TransactionBehavior;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::task;
use tracing::{event, Level};

use crate::db::tags::{SystemTag, TagKind};
use crate::routes::v1::tags::list_tags::TagResponse;

#[derive(Deserialize, Serialize, utoipa::ToSchema)]
pub struct SetEntryTagsRequest {
    /// IDs of the user tags to apply to the entry.
    pub tag_ids: Vec<i64>,
}

#[derive(Deserialize, Serialize, utoipa::ToSchema)]
pub struct GetEntryTagsResponse {
    /// Every tag applied to the entry, both user tags and system tags.
    pub tags: Vec<TagResponse>,
}

#[derive(Error, Debug)]
enum EntryTagsTaskError {
    #[error("entry not found")]
    EntryNotFound,

    #[error("tag not found: {0}")]
    TagNotFound(i64),

    #[error("tag {0} is a system tag")]
    SystemTag(i64),

    #[error("unknown system tag: {0}")]
    UnknownSystemTag(String),

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
}

/// Get entry tags
///
/// Retrieve all tags associated with an entry, including system tags (such as `system:read`).
#[utoipa::path(
    get,
    path = "/v1/entries/id/{id}/tags",
    params(
        ("id" = i64, Path, description = "Entry ID"),
    ),
    responses(
        (status = 200, description = "Tags for the entry", body = GetEntryTagsResponse),
        (status = 404, description = "Entry not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "entries"
)]
#[axum::debug_handler]
pub async fn get_entry_tags(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, Response> {
    let conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;

    let result = task::spawn_blocking(move || {
        // Check if entry exists
        let exists: bool = conn
            .prepare("SELECT EXISTS(SELECT 1 FROM entries WHERE id = ?1)")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row([id], |row| row.get(0))?;

        if !exists {
            return Err(EntryTagsTaskError::EntryNotFound);
        }

        let tags = load_entry_tags(&conn, id)?;

        Ok::<GetEntryTagsResponse, EntryTagsTaskError>(GetEntryTagsResponse { tags })
    })
    .await
    .inspect_err(|e| {
        event!(Level::ERROR, "task error in get_entry_tags: {:?}", e);
    });

    match result {
        Ok(Ok(response)) => Ok(Json(response).into_response()),
        Ok(Err(EntryTagsTaskError::EntryNotFound)) => {
            Err((StatusCode::NOT_FOUND, "Entry not found").into_response())
        }
        Ok(Err(_)) => {
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(_) => Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()),
    }
}

/// Set entry tags
///
/// Set the list of user tags that are applied to an entry. This endpoint replaces all user tags
/// that are currently applied to that entry. System tags cannot be set here, and the entry's
/// system tags are left unchanged; use `/v1/entries/id/{id}/system-tags/{name}` for those.
#[utoipa::path(
    put,
    path = "/v1/entries/id/{id}/tags",
    params(
        ("id" = i64, Path, description = "Entry ID"),
    ),
    request_body = SetEntryTagsRequest,
    responses(
        (status = 200, description = "Tags updated for the entry", body = GetEntryTagsResponse),
        (status = 400, description = "One or more tag IDs not found, or are system tags"),
        (status = 404, description = "Entry not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "entries"
)]
#[axum::debug_handler]
pub async fn set_entry_tags(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(payload): Json<SetEntryTagsRequest>,
) -> Result<Response, Response> {
    let mut conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;

    let result = task::spawn_blocking(move || {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to begin transaction: {:?}", e);
            })?;

        // Check if entry exists
        let exists: bool = tx
            .prepare("SELECT EXISTS(SELECT 1 FROM entries WHERE id = ?1)")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row([id], |row| row.get(0))?;

        if !exists {
            return Err(EntryTagsTaskError::EntryNotFound);
        }

        // Validate all tag IDs exist and are user tags
        for &tag_id in &payload.tag_ids {
            let kind = tx
                .prepare("SELECT kind FROM tags WHERE id = ?1")
                .inspect_err(|e| {
                    event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
                })?
                .query_row([tag_id], |row| row.get::<_, TagKind>(0));

            match kind {
                Ok(TagKind::User) => {}
                Ok(TagKind::System) => return Err(EntryTagsTaskError::SystemTag(tag_id)),
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    return Err(EntryTagsTaskError::TagNotFound(tag_id))
                }
                Err(e) => return Err(e.into()),
            }
        }

        // Delete existing user tag associations
        tx.prepare(
            "DELETE FROM entry_tags WHERE entry_id = ?1
             AND tag_id IN (SELECT id FROM tags WHERE kind = 'user')",
        )
        .inspect_err(|e| {
            event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
        })?
        .execute([id])?;

        // Insert new associations
        {
            let mut stmt = tx
                .prepare("INSERT OR IGNORE INTO entry_tags (entry_id, tag_id) VALUES (?1, ?2)")
                .inspect_err(|e| {
                    event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
                })?;

            for &tag_id in &payload.tag_ids {
                stmt.execute(rusqlite::params![id, tag_id])?;
            }
        }

        // Return the updated tags
        let tags = load_entry_tags(&tx, id)?;

        tx.commit().inspect_err(|e| {
            event!(Level::ERROR, "unable to commit transaction: {:?}", e);
        })?;

        Ok::<GetEntryTagsResponse, EntryTagsTaskError>(GetEntryTagsResponse { tags })
    })
    .await
    .inspect_err(|e| {
        event!(Level::ERROR, "task error in set_entry_tags: {:?}", e);
    });

    match result {
        Ok(Ok(response)) => Ok(Json(response).into_response()),
        Ok(Err(EntryTagsTaskError::EntryNotFound)) => {
            Err((StatusCode::NOT_FOUND, "Entry not found").into_response())
        }
        Ok(Err(EntryTagsTaskError::TagNotFound(tag_id))) => Err((
            StatusCode::BAD_REQUEST,
            format!("Tag not found: {}", tag_id),
        )
            .into_response()),
        Ok(Err(EntryTagsTaskError::SystemTag(tag_id))) => Err((
            StatusCode::BAD_REQUEST,
            format!(
                "Tag {} is a system tag; use /v1/entries/id/{{id}}/system-tags/{{name}} instead",
                tag_id
            ),
        )
            .into_response()),
        Ok(Err(_)) => {
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(_) => Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()),
    }
}

/// Add a system tag to an entry
///
/// Apply a system tag to an entry, e.g. to mark it as read. `name` is the system tag's name,
/// with or without its `system:` prefix (`read`, `saved`, or `hidden`). Adding a tag the entry
/// already has is a no-op.
#[utoipa::path(
    put,
    path = "/v1/entries/id/{id}/system-tags/{name}",
    params(
        ("id" = i64, Path, description = "Entry ID"),
        ("name" = String, Path, description = "System tag name: read, saved, or hidden"),
    ),
    responses(
        (status = 200, description = "Tags for the entry", body = GetEntryTagsResponse),
        (status = 404, description = "Entry or system tag not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "entries"
)]
#[axum::debug_handler]
pub async fn add_entry_system_tag(
    State(state): State<AppState>,
    Path((id, name)): Path<(i64, String)>,
) -> Result<Response, Response> {
    update_entry_system_tag(state, id, name, true).await
}

/// Remove a system tag from an entry
///
/// Remove a system tag from an entry, e.g. to mark it as unread. `name` is the system tag's
/// name, with or without its `system:` prefix (`read`, `saved`, or `hidden`). Removing a tag the
/// entry does not have is a no-op.
#[utoipa::path(
    delete,
    path = "/v1/entries/id/{id}/system-tags/{name}",
    params(
        ("id" = i64, Path, description = "Entry ID"),
        ("name" = String, Path, description = "System tag name: read, saved, or hidden"),
    ),
    responses(
        (status = 200, description = "Tags for the entry", body = GetEntryTagsResponse),
        (status = 404, description = "Entry or system tag not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "entries"
)]
#[axum::debug_handler]
pub async fn remove_entry_system_tag(
    State(state): State<AppState>,
    Path((id, name)): Path<(i64, String)>,
) -> Result<Response, Response> {
    update_entry_system_tag(state, id, name, false).await
}

#[derive(Debug, Default, Deserialize, Serialize, utoipa::ToSchema)]
pub struct BulkSystemTagRequest {
    /// Only tag entries with an ID no greater than this, e.g. the newest entry the user has
    /// seen, so that entries fetched since are left alone.
    #[serde(default)]
    pub up_to_id: Option<i64>,
    /// Only tag entries from this feed.
    #[serde(default)]
    pub feed_id: Option<i64>,
}

#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
pub struct BulkSystemTagResponse {
    /// Number of entries that did not have the tag before, and now do.
    pub tagged: usize,
}

/// Add a system tag to many entries
///
/// Apply a system tag to every entry matching the request, e.g. to mark all entries as read.
/// `name` is the system tag's name, with or without its `system:` prefix (`read`, `saved`, or
/// `hidden`). An empty request body (`{}`) tags every entry; `up_to_id` and `feed_id` narrow it
/// down. Entries that already have the tag are left as they are.
#[utoipa::path(
    put,
    path = "/v1/entries/system-tags/{name}",
    params(
        ("name" = String, Path, description = "System tag name: read, saved, or hidden"),
    ),
    request_body = BulkSystemTagRequest,
    responses(
        (status = 200, description = "Entries tagged", body = BulkSystemTagResponse),
        (status = 404, description = "System tag not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "entries"
)]
#[axum::debug_handler]
pub async fn add_entries_system_tag(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(request): Json<BulkSystemTagRequest>,
) -> Result<Response, Response> {
    let conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;

    let result = task::spawn_blocking(move || {
        let system_tag: SystemTag = name
            .parse()
            .map_err(|_| EntryTagsTaskError::UnknownSystemTag(name))?;
        let tag_id = system_tag.id(&conn)?;
        let tagged = conn
            .prepare(
                "INSERT OR IGNORE INTO entry_tags (entry_id, tag_id)
                 SELECT id, ?1 FROM entries
                 WHERE (?2 IS NULL OR id <= ?2) AND (?3 IS NULL OR feed_id = ?3)",
            )
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .execute(rusqlite::params![tag_id, request.up_to_id, request.feed_id])?;
        Ok::<BulkSystemTagResponse, EntryTagsTaskError>(BulkSystemTagResponse { tagged })
    })
    .await
    .inspect_err(|e| {
        event!(
            Level::ERROR,
            "task error in add_entries_system_tag: {:?}",
            e
        );
    });

    match result {
        Ok(Ok(response)) => Ok(Json(response).into_response()),
        Ok(Err(EntryTagsTaskError::UnknownSystemTag(name))) => Err((
            StatusCode::NOT_FOUND,
            format!("Unknown system tag: {}", name),
        )
            .into_response()),
        Ok(Err(e)) => {
            event!(Level::ERROR, "error in add_entries_system_tag: {:?}", e);
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(_) => Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()),
    }
}

/// Add (`add == true`) or remove the system tag `name` on entry `id`, and
/// respond with the entry's tags.
async fn update_entry_system_tag(
    state: AppState,
    id: i64,
    name: String,
    add: bool,
) -> Result<Response, Response> {
    let conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;

    let result = task::spawn_blocking(move || {
        let system_tag: SystemTag = name
            .parse()
            .map_err(|_| EntryTagsTaskError::UnknownSystemTag(name))?;

        let exists: bool = conn
            .prepare("SELECT EXISTS(SELECT 1 FROM entries WHERE id = ?1)")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row([id], |row| row.get(0))?;

        if !exists {
            return Err(EntryTagsTaskError::EntryNotFound);
        }

        let tag_id = system_tag.id(&conn)?;
        let sql = if add {
            "INSERT OR IGNORE INTO entry_tags (entry_id, tag_id) VALUES (?1, ?2)"
        } else {
            "DELETE FROM entry_tags WHERE entry_id = ?1 AND tag_id = ?2"
        };
        conn.prepare(sql)
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .execute([id, tag_id])?;

        let tags = load_entry_tags(&conn, id)?;
        Ok::<GetEntryTagsResponse, EntryTagsTaskError>(GetEntryTagsResponse { tags })
    })
    .await
    .inspect_err(|e| {
        event!(
            Level::ERROR,
            "task error in update_entry_system_tag: {:?}",
            e
        );
    });

    match result {
        Ok(Ok(response)) => Ok(Json(response).into_response()),
        Ok(Err(EntryTagsTaskError::EntryNotFound)) => {
            Err((StatusCode::NOT_FOUND, "Entry not found").into_response())
        }
        Ok(Err(EntryTagsTaskError::UnknownSystemTag(name))) => Err((
            StatusCode::NOT_FOUND,
            format!("Unknown system tag: {}", name),
        )
            .into_response()),
        Ok(Err(e)) => {
            event!(Level::ERROR, "error in update_entry_system_tag: {:?}", e);
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(_) => Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()),
    }
}

/// Load every tag (user and system) applied to entry `id`.
fn load_entry_tags(conn: &rusqlite::Connection, id: i64) -> rusqlite::Result<Vec<TagResponse>> {
    conn.prepare(
        "SELECT t.id, t.name, t.kind FROM tags t
         INNER JOIN entry_tags et ON et.tag_id = t.id
         WHERE et.entry_id = ?1
         ORDER BY t.id",
    )
    .inspect_err(|e| {
        event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
    })?
    .query_map([id], TagResponse::from_row)?
    .collect()
}
