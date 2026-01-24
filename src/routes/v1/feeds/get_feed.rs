use crate::server::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use tokio::task;
use tracing::{event, Level};

#[derive(serde::Deserialize, serde::Serialize)]
pub struct GetFeedError {
    pub id: i64,
    pub message: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
pub struct GetFeedResponse {
    pub id: i64,
    pub title: String,
    pub url: String,
    pub description: Option<String>,
    pub last_checked: Option<String>,
}

/// Route handler for fetching a single feed's information.
#[axum::debug_handler]
pub async fn get_feed(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let conn = state.conn_pool.get().unwrap();

    // The rusqlite interface is synchronous so we must run the INSERT
    // statement on a blocking thread.
    let task_result = task::spawn_blocking(move || {
        let mut stmt = match conn.prepare(
            "SELECT id, title, url, description, last_checked
                FROM feeds WHERE id = ?1 LIMIT 1",
        ) {
            Ok(s) => s,
            Err(e) => {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
                let result = GetFeedError {
                    id,
                    message: "internal error".to_string(),
                };
                return Err((StatusCode::INTERNAL_SERVER_ERROR, Json(result)).into_response());
            }
        };
        let query_result = stmt.query_row([id], |row| {
            let resp = GetFeedResponse {
                id: row.get(0).unwrap(),
                title: row.get(1).unwrap(),
                url: row.get(2).unwrap(),
                description: row.get(3).unwrap(),
                last_checked: row.get(4).unwrap(),
            };
            Ok(resp)
        });
        match query_result {
            Ok(r) => Ok((StatusCode::OK, Json(r)).into_response()),
            Err(_e) => {
                let result = GetFeedError {
                    id,
                    message: "not found".to_string(),
                };
                Err((StatusCode::NOT_FOUND, Json(result)).into_response())
            }
        }
    })
    .await;

    match task_result {
        Ok(Ok(res)) | Ok(Err(res)) => res,
        Err(e) => {
            event!(
                Level::ERROR,
                "error waiting for blocking thread to run SQL query: {:?}",
                e
            );
            let result = GetFeedError {
                id,
                message: "internal error".to_string(),
            };
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(result)).into_response();
        }
    }
}
