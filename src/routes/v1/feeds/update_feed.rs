use crate::http::{FeedAuth, FeedAuthError, FeedAuthType};
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

#[derive(Default, Serialize, Deserialize, utoipa::ToSchema)]
#[cfg_attr(feature = "mcp", derive(schemars::JsonSchema))]
pub struct UpdateFeedRequest {
    pub title: Option<String>,
    pub url: Option<String>,
    pub description: Option<String>,
    /// Minimum interval, in seconds, between fetches of this feed.
    pub min_fetch_interval_seconds: Option<i64>,
    /// Authentication scheme to apply when fetching this feed. Setting
    /// `"none"` clears any existing credentials.
    pub auth_type: Option<FeedAuthType>,
    /// Username for HTTP Basic auth. Omitted fields are left unchanged.
    pub auth_username: Option<String>,
    /// Password for HTTP Basic auth. Omitted fields are left unchanged.
    pub auth_password: Option<String>,
    /// Token for HTTP Bearer auth. Omitted fields are left unchanged.
    pub auth_bearer_token: Option<String>,
}

#[derive(Serialize, Deserialize, utoipa::ToSchema)]
pub struct UpdateFeedResponse {
    pub id: i64,
    pub title: String,
    pub url: String,
    pub description: Option<String>,
    pub min_fetch_interval_seconds: i64,
    /// Current authentication scheme. Credentials are never returned.
    pub auth_type: FeedAuthType,
}

#[derive(Error, Debug)]
enum UpdateFeedTaskError {
    #[error("feed not found")]
    FeedNotFound,

    #[error("invalid update parameters")]
    InvalidUpdate,

    #[error("min_fetch_interval_seconds must be positive")]
    InvalidFetchInterval,

    #[error("{0}")]
    InvalidAuth(#[from] FeedAuthError),

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
}

/// Update a feed
///
/// Update data used to configure a single feed.
#[utoipa::path(
    put,
    path = "/v1/feeds/id/{id}",
    params(
        ("id" = i64, Path, description = "Feed ID"),
    ),
    request_body = UpdateFeedRequest,
    responses(
        (status = 200, description = "Feed updated successfully", body = UpdateFeedResponse),
        (status = 400, description = "No update fields provided"),
        (status = 404, description = "Feed not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "feeds"
)]
#[axum::debug_handler]
pub async fn update_feed(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(payload): Json<UpdateFeedRequest>,
) -> Result<Response, Response> {
    let conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;

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

        if let Some(min_fetch_interval_seconds) = payload.min_fetch_interval_seconds {
            if min_fetch_interval_seconds <= 0 {
                return Err(UpdateFeedTaskError::InvalidFetchInterval);
            }
            updates.push("min_fetch_interval_seconds = ?".to_string());
            params.push(Box::new(min_fetch_interval_seconds));
        }

        // Authentication updates. When `auth_type` is provided we always
        // rewrite the full set of auth columns so a caller that changes
        // schemes (e.g. basic → bearer) doesn't leave stale credentials
        // from the previous scheme behind. When `auth_type` is absent but
        // an individual credential field is set, just patch that field.
        if let Some(new_type) = payload.auth_type {
            let auth = FeedAuth {
                auth_type: new_type,
                username: payload.auth_username.clone(),
                password: payload.auth_password.clone(),
                bearer_token: payload.auth_bearer_token.clone(),
            };
            auth.validate()?;

            updates.push("auth_type = ?".to_string());
            params.push(Box::new(auth.auth_type.as_db().map(str::to_string)));

            // Clear credentials not used by this scheme so we never leave
            // the previous scheme's secrets sitting in the database.
            match auth.auth_type {
                FeedAuthType::None => {
                    updates.push("auth_username = NULL".to_string());
                    updates.push("auth_password = NULL".to_string());
                    updates.push("auth_bearer_token = NULL".to_string());
                }
                FeedAuthType::Basic => {
                    updates.push("auth_username = ?".to_string());
                    params.push(Box::new(auth.username));
                    updates.push("auth_password = ?".to_string());
                    params.push(Box::new(auth.password));
                    updates.push("auth_bearer_token = NULL".to_string());
                }
                FeedAuthType::Bearer => {
                    updates.push("auth_username = NULL".to_string());
                    updates.push("auth_password = NULL".to_string());
                    updates.push("auth_bearer_token = ?".to_string());
                    params.push(Box::new(auth.bearer_token));
                }
            }
        } else {
            if let Some(username) = payload.auth_username {
                updates.push("auth_username = ?".to_string());
                params.push(Box::new(username));
            }
            if let Some(password) = payload.auth_password {
                updates.push("auth_password = ?".to_string());
                params.push(Box::new(password));
            }
            if let Some(token) = payload.auth_bearer_token {
                updates.push("auth_bearer_token = ?".to_string());
                params.push(Box::new(token));
            }
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
                "SELECT id, title, url, description, min_fetch_interval_seconds, auth_type
                FROM feeds WHERE id = ?1 LIMIT 1",
            )
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare select statement: {:?}", e);
            })?
            .query_row([id], |row| {
                let auth_type_raw: Option<String> = row.get(5)?;
                let auth_type = FeedAuthType::from_db(auth_type_raw.as_deref()).unwrap_or_default();
                Ok(UpdateFeedResponse {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    url: row.get(2)?,
                    description: row.get(3)?,
                    min_fetch_interval_seconds: row.get(4)?,
                    auth_type,
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
        Ok(Err(UpdateFeedTaskError::InvalidFetchInterval)) => Err((
            StatusCode::BAD_REQUEST,
            "min_fetch_interval_seconds must be positive",
        )
            .into_response()),
        Ok(Err(UpdateFeedTaskError::InvalidAuth(e))) => {
            Err((StatusCode::BAD_REQUEST, format!("{e}")).into_response())
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
