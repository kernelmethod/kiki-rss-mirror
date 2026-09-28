use crate::db::tags::TagKind;
use crate::server::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use tokio::task;
use tracing::{event, Level};

/// Delete a tag
///
/// Delete a user tag by its ID. System tags cannot be deleted.
#[utoipa::path(
    delete,
    path = "/v1/tags/id/{id}",
    params(
        ("id" = i64, Path, description = "Tag ID"),
    ),
    responses(
        (status = 204, description = "Tag deleted successfully"),
        (status = 403, description = "System tags cannot be deleted"),
        (status = 404, description = "Tag not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "tags"
)]
#[axum::debug_handler]
pub async fn delete_tag(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, Response> {
    let conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;

    let result = task::spawn_blocking(move || {
        let kind = conn
            .prepare("SELECT kind FROM tags WHERE id = ?1")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row([id], |row| row.get::<_, TagKind>(0));

        match kind {
            Ok(TagKind::User) => {}
            Ok(TagKind::System) => return Ok(Some(TagKind::System)),
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(e) => return Err(e),
        }

        conn.prepare("DELETE FROM tags WHERE id = ?1")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .execute([id])?;

        Ok::<Option<TagKind>, rusqlite::Error>(Some(TagKind::User))
    })
    .await
    .inspect_err(|e| {
        event!(Level::ERROR, "task error in delete_tag: {:?}", e);
    });

    match result {
        Ok(Ok(None)) => Ok((StatusCode::NOT_FOUND, "Tag not found").into_response()),
        Ok(Ok(Some(TagKind::System))) => {
            Err((StatusCode::FORBIDDEN, "System tags cannot be deleted").into_response())
        }
        Ok(Ok(Some(TagKind::User))) => Ok((StatusCode::NO_CONTENT, "").into_response()),
        Ok(Err(_)) | Err(_) => {
            event!(Level::ERROR, "an error occurred while running delete_tag");
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
    }
}
