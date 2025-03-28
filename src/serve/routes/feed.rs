use crate::db::ConnectionBuilder;
use axum::{http::StatusCode, routing::post, Json, Router};
use tracing::{event, Level};

pub fn create_router() -> Router {
    Router::new().route("/", post(add_feed))
}

struct AddFeedQueryResult(i64);

#[derive(serde::Serialize)]
struct AddFeedResult {
    id: i64,
}

/// Route handler for adding a new feed to Kiki.
#[axum::debug_handler]
async fn add_feed() -> (StatusCode, Json<AddFeedResult>) {
    // Add a new feed instance to the database
    let builder = ConnectionBuilder::default().read_write();
    let conn = match builder.build() {
        Ok(c) => c,
        Err(e) => {
            event!(Level::ERROR, "unable to open database connection: {:?}", e);
            let result = AddFeedResult { id: 0 };
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(result));
        }
    };

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
