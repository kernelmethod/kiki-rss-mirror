use crate::server::AppState;
use axum::{
    extract::{Path, State},
    response::{IntoResponse, Response},
};
use tokio::task;
use tracing::{event, Level};

/// Delete an entry
///
/// Delete an entry by its ID.
#[utoipa::path(
    delete,
    path = "/v1/entries/id/{id}",
    params(
        ("id" = i64, Path, description = "Entry ID"),
    ),
    responses(
        (status = 204, description = "Entry deleted successfully"),
        (status = 404, description = "Entry not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "entries"
)]
pub async fn delete_entry(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, Response> {
    let conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "Internal error",
        )
            .into_response()
    })?;

    let result = task::spawn_blocking(move || {
        let changes = conn
            .prepare("DELETE FROM entries WHERE id = ?1")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .execute([id])
            .inspect_err(|e| {
                event!(Level::ERROR, "failed to delete entry: {:?}", e);
            })?;

        Ok::<usize, rusqlite::Error>(changes)
    })
    .await;

    match result {
        Ok(Ok(changes)) => {
            if changes == 0 {
                // No rows were deleted, meaning the entry doesn't exist
                Err((axum::http::StatusCode::NOT_FOUND, "Entry not found").into_response())
            } else {
                // Successfully deleted the entry
                Ok((axum::http::StatusCode::NO_CONTENT, "").into_response())
            }
        }
        Ok(Err(e)) => {
            event!(Level::ERROR, "error in delete_entry: {:?}", e);
            Err((
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Internal error",
            )
                .into_response())
        }
        Err(e) => {
            event!(Level::ERROR, "task error in delete_entry: {:?}", e);
            Err((
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Internal error",
            )
                .into_response())
        }
    }
}
