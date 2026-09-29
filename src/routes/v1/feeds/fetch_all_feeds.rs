use crate::server::AppState;
use crate::tasks::TaskManagerCommand;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use tokio::task;
use tracing::{event, Level};

#[derive(Serialize, utoipa::ToSchema)]
pub struct FetchAllFeedsResponse {
    pub queued: usize,
}

/// Refresh all feeds
///
/// Queue requests to refresh all of the feeds that kiki is configured to read from. This will
/// force a refresh even for feeds that kiki has updated recently, except those whose server has
/// asked kiki to wait with a `Retry-After` header that has not yet passed.
#[utoipa::path(
    post,
    path = "/v1/feeds/refresh",
    responses(
        (status = 202, description = "All feeds queued for refresh", body = FetchAllFeedsResponse),
        (status = 500, description = "Internal server error"),
    ),
    tag = "feeds"
)]
#[axum::debug_handler]
pub async fn fetch_all_feeds(State(state): State<AppState>) -> Result<Response, Response> {
    let conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;

    // Collect all feed IDs
    let feed_ids = task::spawn_blocking(move || {
        let ids = conn
            .prepare("SELECT id FROM feeds")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_map([], |row| row.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;

        Ok::<Vec<i64>, rusqlite::Error>(ids)
    })
    .await
    .inspect_err(|e| {
        event!(Level::ERROR, "task error in fetch_all_feeds: {:?}", e);
    });

    let feed_ids = match feed_ids {
        Ok(Ok(ids)) => ids,
        _ => {
            return Err(
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
            );
        }
    };

    // Queue refresh commands for each feed
    let mut queued = 0;
    for id in &feed_ids {
        if let Err(e) = state
            .task_manager_tx
            .send(TaskManagerCommand::RefreshFeed {
                feed_id: *id,
                manual: true,
            })
            .await
        {
            event!(
                Level::ERROR,
                "failed to send fetch command for feed {}: {:?}",
                id,
                e
            );
        } else {
            state.metrics.record_task_enqueued("refresh_feed");
            queued += 1;
        }
    }

    event!(Level::INFO, "queued {} feeds for refresh", queued);
    Ok((StatusCode::ACCEPTED, Json(FetchAllFeedsResponse { queued })).into_response())
}
