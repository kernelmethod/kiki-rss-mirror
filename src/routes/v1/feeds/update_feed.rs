use crate::server::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::task;
use tracing::{event, Level};

#[derive(Serialize, Deserialize)]
pub struct UpdateFeedRequest {
    pub title: Option<String>,
    pub url: Option<String>,
    pub description: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct UpdateFeedResponse {
    pub id: i64,
    pub title: String,
    pub url: String,
    pub description: Option<String>,
}

#[derive(Error, Debug)]
enum UpdateFeedTaskError {
    #[error("feed not found")]
    FeedNotFound,

    #[error("invalid update parameters")]
    InvalidUpdate,

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
}

/// Route handler for updating a feed.
#[axum::debug_handler]
pub async fn update_feed(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(payload): Json<UpdateFeedRequest>,
) -> Result<Response, Response> {
    let conn = state.conn_pool.get().unwrap();

    let result = task::spawn_blocking(move || {
        // First, check if the feed exists
        let exists: bool = conn
            .prepare("SELECT EXISTS(SELECT 1 FROM feeds WHERE id = ?1)")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row([id], |row| row.get(0))?;

        if !exists {
            return Err(UpdateFeedTaskError::FeedNotFound);
        }

        // Build the update query dynamically based on what fields are provided
        let mut updates = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![];

        if let Some(title) = &payload.title {
            updates.push("title = ?".to_string());
            params.push(Box::new(title.clone()));
        }

        if let Some(url) = &payload.url {
            updates.push("url = ?".to_string());
            params.push(Box::new(url.clone()));
        }

        if let Some(description) = &payload.description {
            updates.push("description = ?".to_string());
            params.push(Box::new(description.clone()));
        }

        if updates.is_empty() {
            // No updates provided
            return Err(UpdateFeedTaskError::InvalidUpdate);
        }

        // Construct the full query
        params.push(Box::new(id));
        let query = format!("UPDATE feeds SET {} WHERE id = ?", updates.join(", "));

        // Execute the update
        let params = rusqlite::params_from_iter(params);
        conn.execute(&query, params).inspect_err(|e| {
            event!(Level::ERROR, "unable to execute update statement: {:?}", e);
        })?;

        // Retrieve the updated feed data
        let feed = conn
            .prepare(
                "SELECT id, title, url, description
                FROM feeds WHERE id = ?1 LIMIT 1",
            )
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare select statement: {:?}", e);
            })?
            .query_row([id], |row| {
                Ok(UpdateFeedResponse {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    url: row.get(2)?,
                    description: row.get(3)?,
                })
            })?;

        Ok::<UpdateFeedResponse, UpdateFeedTaskError>(feed)
    })
    .await
    .inspect_err(|e| {
        event!(Level::ERROR, "task error in update_feed: {:?}", e);
    });

    match result {
        Ok(Ok(feed)) => Ok(Json(feed).into_response()),
        Ok(Err(UpdateFeedTaskError::FeedNotFound)) => {
            Err((StatusCode::NOT_FOUND, "Feed not found").into_response())
        }
        Ok(Err(UpdateFeedTaskError::InvalidUpdate)) => {
            Err((StatusCode::BAD_REQUEST, "Invalid update parameters").into_response())
        }
        Ok(Err(UpdateFeedTaskError::Database(_))) => {
            event!(Level::ERROR, "database error in update_feed");
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(_) => {
            event!(Level::ERROR, "an error occurred while running update_feed");
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
    }
}
