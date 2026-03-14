use crate::server::AppState;
use crate::tasks::FetchError;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use tokio::task;
use tracing::{event, Level};

#[derive(serde::Deserialize, serde::Serialize, utoipa::ToSchema)]
pub struct GetFeedResponse {
    pub id: i64,
    pub title: String,
    pub url: String,
    pub description: Option<String>,
    /// Last checked time in RFC3339 format.
    pub last_checked: Option<String>,
    /// Most recent fetch error, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_fetch_error: Option<FetchError>,
    /// Time of the most recent fetch error in RFC3339 format.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_fetch_error_at: Option<String>,
}

/// Get feed information
///
/// Retrieve information about a single feed by that feed's ID.
#[utoipa::path(
    get,
    path = "/v1/feeds/id/{id}",
    params(
        ("id" = i64, Path, description = "Feed ID"),
    ),
    responses(
        (status = 200, description = "Feed found", body = GetFeedResponse),
        (status = 404, description = "Feed not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "feeds"
)]
#[axum::debug_handler]
pub async fn get_feed(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let conn = match state.conn_pool.get() {
        Ok(conn) => conn,
        Err(e) => {
            event!(Level::ERROR, "failed to get database connection: {:?}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response();
        }
    };

    // The rusqlite interface is synchronous so we must run the INSERT
    // statement on a blocking thread.
    let task_result = task::spawn_blocking(move || {
        let mut stmt = match conn.prepare(
            "SELECT id, title, url, description, last_checked, last_fetch_error, last_fetch_error_at
                FROM feeds WHERE id = ?1 LIMIT 1",
        ) {
            Ok(s) => s,
            Err(e) => {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
                return (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response();
            }
        };
        let query_result = stmt.query_row([id], |row| {
            let resp = GetFeedResponse {
                id: row.get(0)?,
                title: row.get(1)?,
                url: row.get(2)?,
                description: row.get(3)?,
                last_checked: row.get::<usize, Option<i64>>(4)?.and_then(|ts| {
                    chrono::DateTime::from_timestamp_secs(ts).map(|d| d.to_rfc3339())
                }),
                last_fetch_error: row
                    .get::<usize, Option<String>>(5)?
                    .and_then(|s| serde_json::from_str(&s).ok()),
                last_fetch_error_at: row.get::<usize, Option<i64>>(6)?.and_then(|ts| {
                    chrono::DateTime::from_timestamp_secs(ts).map(|d| d.to_rfc3339())
                }),
            };
            Ok(resp)
        });
        match query_result {
            Ok(r) => (StatusCode::OK, Json(r)).into_response(),
            Err(_e) => (StatusCode::NOT_FOUND, "Feed not found").into_response(),
        }
    })
    .await;

    match task_result {
        Ok(res) => res,
        Err(e) => {
            event!(
                Level::ERROR,
                "error waiting for blocking thread to run SQL query: {:?}",
                e
            );
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
        }
    }
}
