use crate::server::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use tokio::task;
use tracing::{event, Level};

#[derive(Deserialize, utoipa::IntoParams)]
pub struct DeleteFeedParams {
    /// Whether to also delete all entries associated with the feed (default: true).
    pub delete_entries: Option<bool>,
}

/// Delete a feed
///
/// Delete a feed and all entries associated with that feed.
#[utoipa::path(
    delete,
    path = "/v1/feeds/id/{id}",
    params(
        ("id" = i64, Path, description = "Feed ID"),
        DeleteFeedParams,
    ),
    responses(
        (status = 204, description = "Feed deleted successfully"),
        (status = 404, description = "Feed not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "feeds"
)]
#[axum::debug_handler]
pub async fn delete_feed(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(params): Query<DeleteFeedParams>,
) -> Result<Response, Response> {
    let conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;

    let delete_entries = params.delete_entries.unwrap_or(true);

    let result = task::spawn_blocking(move || {
        if delete_entries {
            conn.execute("DELETE FROM entries WHERE feed_id = ?1", [id])?;
        }

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
