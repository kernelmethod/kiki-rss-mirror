use crate::server::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use rusqlite::TransactionBehavior;
use serde::Deserialize;
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
    let delete_entries = params.delete_entries.unwrap_or(true);

    let result = state
        .db
        .write(move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .inspect_err(|e| {
                    event!(Level::ERROR, "unable to begin transaction: {:?}", e);
                })?;

            // Capture the pre-delete feed identity so we can report it in the
            // `feed.removed` event after the transaction commits.
            let feed_identity = tx
                .query_row("SELECT url, title FROM feeds WHERE id = ?1", [id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .ok();

            if delete_entries {
                tx.execute("DELETE FROM entries WHERE feed_id = ?1", [id])?;
            }

            let affected_rows = tx
                .prepare("DELETE FROM feeds WHERE id = ?1")
                .inspect_err(|e| {
                    event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
                })?
                .execute([id])?;

            tx.commit()?;

            Ok::<(usize, Option<(String, String)>), rusqlite::Error>((affected_rows, feed_identity))
        })
        .await
        .inspect_err(|e| {
            event!(Level::ERROR, "task error in delete_feed: {:?}", e);
        });

    match result {
        Ok(Ok((0, _))) => Ok((StatusCode::NOT_FOUND, "Feed not found").into_response()),
        Ok(Ok((_, identity))) => {
            if let (Some(runner), Some((url, title))) = (state.script_runner.current(), identity) {
                runner.dispatch_observe(
                    crate::scripting::Event::FeedRemoved,
                    crate::scripting::EventPayload::Feed { id, url, title },
                );
            }
            Ok((StatusCode::NO_CONTENT, "").into_response())
        }
        Ok(Err(_)) | Err(_) => {
            event!(Level::ERROR, "an error occurred while running delete_feed");
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
    }
}
