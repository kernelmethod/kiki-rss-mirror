use crate::opml;
use crate::server::AppState;
use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use tokio::task;
use tracing::{event, Level};

/// Export feeds as OPML
///
/// Export all feeds as [OPML](https://en.wikipedia.org/wiki/OPML) so that they may be imported
/// into another RSS feed aggregator.
#[utoipa::path(
    get,
    path = "/v1/feeds/export",
    responses(
        (status = 200, description = "OPML export of all feeds", content_type = "application/xml"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "feeds"
)]
#[axum::debug_handler]
pub async fn export_opml(State(state): State<AppState>) -> Result<Response, Response> {
    let internal_error =
        || (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response();

    let conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        internal_error()
    })?;

    let xml = task::spawn_blocking(move || opml::build_opml(&opml::export_feeds(&conn)?))
        .await
        .map_err(|e| {
            event!(Level::ERROR, "task error in export_opml: {:?}", e);
            internal_error()
        })?
        .map_err(|e| {
            event!(Level::ERROR, "failed to export OPML: {:?}", e);
            internal_error()
        })?;

    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/xml")],
        xml,
    )
        .into_response())
}
