use crate::serve::server::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use std::sync::Arc;
use tokio::task;
use tracing::{event, Level};

pub fn create_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", post(add_feed))
        .route("/{*id}", get(get_feed))
}

struct AddFeedQueryResult(i64);

#[derive(serde::Serialize)]
struct AddFeedResponse {
    id: i64,
}

/// Route handler for adding a new feed to Kiki.
#[axum::debug_handler]
async fn add_feed(State(state): State<Arc<AppState>>) -> (StatusCode, Json<AddFeedResponse>) {
    // Add a new feed instance to the database
    let conn = state.conn_pool.get().unwrap();

    // The rusqlite interface is synchronous so we must run the INSERT statement
    // on a blocking thread.
    let task_result = task::spawn_blocking(move || {
        let mut stmt =
            match conn.prepare("INSERT INTO feeds (title, url) VALUES (?1, ?2) RETURNING id") {
                Ok(s) => s,
                Err(e) => {
                    event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
                    let result = AddFeedResponse { id: 0 };
                    return Err((StatusCode::INTERNAL_SERVER_ERROR, Json(result)));
                }
            };
        let insert_result = stmt
            .query_row(["my feed", "https://kernelmethod.org/rss.xml"], |row| {
                Ok(AddFeedQueryResult(row.get(0).unwrap()))
            });
        match insert_result {
            Ok(r) => Ok(r.0),
            Err(e) => {
                event!(
                    Level::ERROR,
                    "failure while adding new feed to database: {:?}",
                    e
                );
                let result = AddFeedResponse { id: 0 };
                return Err((StatusCode::INTERNAL_SERVER_ERROR, Json(result)));
            }
        }
    })
    .await;

    let id = match task_result {
        Ok(Ok(id)) => id,
        Ok(Err(resp)) => return resp,
        Err(e) => {
            event!(
                Level::ERROR,
                "error waiting for blocking thread to run SQL query: {:?}",
                e
            );
            let result = AddFeedResponse { id: 0 };
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(result));
        }
    };

    event!(Level::INFO, "created new feed");
    let result = AddFeedResponse { id };

    (StatusCode::CREATED, Json(result))
}

/// Route handler for fetching a single feed's information.
#[axum::debug_handler]
async fn get_feed(
    State(_state): State<Arc<AppState>>,
    Path(_id): Path<u64>,
) -> (StatusCode, &'static str) {
    (StatusCode::OK, "")
}

#[cfg(test)]
mod test {}
