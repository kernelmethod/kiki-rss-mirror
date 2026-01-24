use crate::server::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use tokio::task;
use tracing::{event, Level};

/// Route handler for deleting a feed.
#[axum::debug_handler]
pub async fn delete_feed(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, Response> {
    let conn = state.conn_pool.get().unwrap();

    let result = task::spawn_blocking(move || {
        let affected_rows = conn
            .prepare("DELETE FROM feeds WHERE id = ?1")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .execute([id])?;

        Ok::<usize, rusqlite::Error>(affected_rows)
    })
    .await
    .inspect_err(|e| {
        event!(Level::ERROR, "task error in delete_feed: {:?}", e);
    });

    match result {
        Ok(Ok(0)) => Ok((StatusCode::NOT_FOUND, "Feed not found").into_response()),
        Ok(Ok(_)) => Ok((StatusCode::NO_CONTENT, "").into_response()),
        Ok(Err(_)) | Err(_) => {
            event!(Level::ERROR, "an error occurred while running delete_feed");
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
    }
}
