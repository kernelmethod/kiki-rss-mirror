use crate::serve::server::AppState;
use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use std::sync::Arc;
use tracing::{event, Level};

pub fn create_router() -> Router<Arc<AppState>> {
    Router::new().route("/", post(add_feed))
}

struct AddFeedQueryResult(i64);

#[derive(serde::Serialize)]
struct AddFeedResult {
    id: i64,
}

/// Route handler for adding a new feed to Kiki.
#[axum::debug_handler]
async fn add_feed(State(state): State<Arc<AppState>>) -> (StatusCode, Json<AddFeedResult>) {
    // Add a new feed instance to the database
    let conn = state.conn_pool.get().unwrap();
    let mut stmt = match conn.prepare("INSERT INTO feeds (title, url) VALUES (?1, ?2) RETURNING id")
    {
        Ok(s) => s,
        Err(e) => {
            event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            let result = AddFeedResult { id: 0 };
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(result));
        }
    };
    let insert_result = stmt.query_row(["my feed", "https://kernelmethod.org/rss.xml"], |row| {
        Ok(AddFeedQueryResult(row.get(0).unwrap()))
    });
    let id = match insert_result {
        Ok(r) => r.0,
        Err(e) => {
            event!(
                Level::ERROR,
                "failure while adding new feed to database: {:?}",
                e
            );
            let result = AddFeedResult { id: 0 };
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(result));
        }
    };

    event!(Level::INFO, "created new feed");
    let result = AddFeedResult { id };

    (StatusCode::CREATED, Json(result))
}
