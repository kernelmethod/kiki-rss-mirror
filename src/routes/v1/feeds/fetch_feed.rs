use crate::fetcher::FetchManagerCommand;
use crate::server::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use tracing::{event, Level};

/// Route handler for fetching a single feed.
#[utoipa::path(
    post,
    path = "/v1/feeds/fetch/{id}",
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
    if let Err(e) = state
        .fetcher_tx
        .send(FetchManagerCommand::RefreshFeed(id))
        .await
    {
        event!(
            Level::ERROR,
            "failed to send fetch command for feed {}: {:?}",
            id,
            e
        );
        return (StatusCode::INTERNAL_SERVER_ERROR, "Failed to queue fetch").into_response();
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
