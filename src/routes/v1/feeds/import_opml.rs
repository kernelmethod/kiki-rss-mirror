use crate::opml;
use crate::server::AppState;
use crate::tasks::TaskManagerCommand;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use tokio::task;
use tracing::{event, Level};

#[derive(Serialize, Deserialize, utoipa::ToSchema)]
pub struct ImportOpmlResponse {
    pub imported: usize,
}

/// Import feeds from OPML
///
/// Import feeds from [OPML](https://en.wikipedia.org/wiki/OPML) format to start retrieving content
/// from that feed. Folders in the OPML become tags on the feeds inside them. Feeds whose URL
/// already exists are skipped, except that they gain the tags of the folders they're in, keeping
/// the tags they have.
#[utoipa::path(
    post,
    path = "/v1/feeds/import",
    request_body(content = String, content_type = "application/xml", description = "OPML XML data"),
    responses(
        (status = 201, description = "Feeds imported successfully", body = ImportOpmlResponse),
        (status = 200, description = "No feeds imported (empty OPML)", body = ImportOpmlResponse),
        (status = 400, description = "Invalid OPML format"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "feeds"
)]
#[axum::debug_handler]
pub async fn import_opml(
    State(state): State<AppState>,
    body: String,
) -> Result<Response, Response> {
    let internal_error =
        || (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response();

    // Parse OPML to extract feeds
    let feeds = opml::parse_opml(&body).map_err(|e| {
        event!(Level::ERROR, "failed to parse OPML: {:?}", e);
        (StatusCode::BAD_REQUEST, "Invalid OPML format").into_response()
    })?;

    if feeds.is_empty() {
        return Ok((StatusCode::OK, Json(ImportOpmlResponse { imported: 0 })).into_response());
    }

    let mut conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        internal_error()
    })?;

    let fetch_interval = state
        .config
        .current()
        .feed_fetch
        .default_fetch_interval_seconds;
    let summary =
        task::spawn_blocking(move || opml::import_feeds(&mut conn, &feeds, fetch_interval))
            .await
            .map_err(|e| {
                event!(Level::ERROR, "task error in import_opml: {:?}", e);
                internal_error()
            })?
            .map_err(|e| {
                event!(Level::ERROR, "failed to import OPML: {:?}", e);
                internal_error()
            })?;

    // Queue fetches for all new feeds
    for &id in &summary.imported {
        if let Err(e) = state
            .task_manager_tx
            .send(TaskManagerCommand::RefreshFeed {
                feed_id: id,
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
        }
    }

    Ok((
        StatusCode::CREATED,
        Json(ImportOpmlResponse {
            imported: summary.imported.len(),
        }),
    )
        .into_response())
}
