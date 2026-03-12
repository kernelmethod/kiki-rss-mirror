use crate::fetcher::FetchManagerCommand;
use crate::server::AppState;
use axum::{extract::State, http::StatusCode, Json};
use tokio::task;
use tracing::{event, Level};

struct AddFeedQueryResult(i64);

#[derive(serde::Deserialize, serde::Serialize)]
pub struct AddFeedRequest {
    pub title: String,
    pub url: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
pub struct AddFeedResponse {
    pub id: i64,
}

/// Route handler for adding a new feed.
#[axum::debug_handler]
pub async fn add_feed(
    State(state): State<AppState>,
    Json(payload): Json<AddFeedRequest>,
) -> (StatusCode, Json<AddFeedResponse>) {
    // Add a new feed instance to the database
    let conn = match state.conn_pool.get() {
        Ok(conn) => conn,
        Err(e) => {
            event!(Level::ERROR, "failed to get database connection: {:?}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(AddFeedResponse { id: 0 }),
            );
        }
    };

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
        let insert_result = stmt.query_row([payload.title, payload.url], |row| {
            Ok(AddFeedQueryResult(row.get(0)?))
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
                Err((StatusCode::INTERNAL_SERVER_ERROR, Json(result)))
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

    // Issue a command to the feed-fetch workers to make them fetch
    // the latest version of the feed.
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
    }

    (StatusCode::CREATED, Json(result))
}
