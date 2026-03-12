use crate::fetcher::FetchManagerCommand;
use crate::server::AppState;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use tokio::task;
use tracing::{event, Level};

#[derive(Serialize)]
pub struct FetchAllFeedsResponse {
    pub queued: usize,
}

/// Route handler for triggering a refresh of all feeds.
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
            .fetcher_tx
            .send(FetchManagerCommand::RefreshFeed(*id))
            .await
        {
            event!(
                Level::ERROR,
                "failed to send fetch command for feed {}: {:?}",
                id,
                e
            );
        } else {
            queued += 1;
        }
    }

    event!(Level::INFO, "queued {} feeds for refresh", queued);
    Ok((StatusCode::ACCEPTED, Json(FetchAllFeedsResponse { queued })).into_response())
}
