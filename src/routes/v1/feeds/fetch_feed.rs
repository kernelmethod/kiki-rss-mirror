use crate::server::AppState;
use crate::tasks::{Enqueue, TaskManagerCommand};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use tracing::{event, Level};

/// Refresh single feed
///
/// Queue a request to refresh an individual feed by its ID. This will force a refresh even if kiki
/// has updated the feed recently, unless the feed's server has asked kiki to wait with a
/// `Retry-After` header that has not yet passed.
#[utoipa::path(
    post,
    path = "/v1/feeds/refresh/{id}",
    params(
        ("id" = i64, Path, description = "Feed ID"),
    ),
    responses(
        (status = 202, description = "Fetch queued successfully"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "feeds"
)]
#[axum::debug_handler]
pub async fn fetch_feed(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    // Send a command to the feed fetcher workers to refresh this feed
    // A refresh of the feed that is already queued answers this request
    // too, so it is not queued twice.
    match state
        .task_manager_tx
        .send(TaskManagerCommand::RefreshFeed {
            feed_id: id,
            manual: true,
        })
        .await
    {
        Ok(Enqueue::Queued) => state.metrics.record_task_enqueued("refresh_feed"),
        Ok(Enqueue::AlreadyQueued) => {}
        Err(e) => {
            event!(
                Level::ERROR,
                "failed to send fetch command for feed {}: {:?}",
                id,
                e
            );
            return (StatusCode::INTERNAL_SERVER_ERROR, "Failed to queue fetch").into_response();
        }
    }

    event!(
        Level::INFO,
        "initiated feed fetch request for feed id: {:?}",
        id
    );

    // Return a 202 Accepted status to indicate the request has been accepted
    // for processing but not yet completed
    (StatusCode::ACCEPTED, "").into_response()
}
