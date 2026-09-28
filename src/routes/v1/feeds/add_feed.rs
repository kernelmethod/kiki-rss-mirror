use crate::http::{FeedAuth, FeedAuthType};
use crate::server::AppState;
use crate::tasks::TaskManagerCommand;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use tokio::task;
use tracing::{event, Level};

struct AddFeedQueryResult(i64);

#[derive(Default, serde::Deserialize, serde::Serialize, utoipa::ToSchema)]
pub struct AddFeedRequest {
    pub title: String,
    pub url: String,
    /// Authentication scheme to apply when fetching this feed. One of
    /// `"none"` (default), `"basic"`, or `"bearer"`. Omit or set to `"none"`
    /// for unauthenticated feeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_type: Option<FeedAuthType>,
    /// Username for HTTP Basic auth. Required when `auth_type` is `"basic"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_username: Option<String>,
    /// Password for HTTP Basic auth. Optional even for `"basic"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_password: Option<String>,
    /// Token for HTTP Bearer auth. Required when `auth_type` is `"bearer"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_bearer_token: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize, utoipa::ToSchema)]
pub struct AddFeedResponse {
    pub id: i64,
}

/// Add a new feed
///
/// Register a new feed from which to fetch content.
#[utoipa::path(
    post,
    path = "/v1/feeds/create",
    request_body = AddFeedRequest,
    responses(
        (status = 201, description = "Feed created successfully", body = AddFeedResponse),
        (status = 400, description = "Invalid authentication parameters"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "feeds"
)]
#[axum::debug_handler]
pub async fn add_feed(
    State(state): State<AppState>,
    Json(payload): Json<AddFeedRequest>,
) -> Response {
    let auth = FeedAuth {
        auth_type: payload.auth_type.unwrap_or_default(),
        username: payload.auth_username,
        password: payload.auth_password,
        bearer_token: payload.auth_bearer_token,
    };
    if let Err(e) = auth.validate() {
        return (StatusCode::BAD_REQUEST, format!("{e}")).into_response();
    }

    let conn = match state.conn_pool.get() {
        Ok(conn) => conn,
        Err(e) => {
            event!(Level::ERROR, "failed to get database connection: {:?}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response();
        }
    };

    let title = payload.title;
    let url = payload.url;
    let fetch_interval = state
        .config
        .current()
        .feed_fetch
        .default_fetch_interval_seconds;
    let title_for_event = title.clone();
    let url_for_event = url.clone();

    // The rusqlite interface is synchronous so we must run the INSERT statement
    // on a blocking thread.
    let task_result = task::spawn_blocking(move || -> Result<i64, rusqlite::Error> {
        let mut stmt = conn.prepare(
            "INSERT INTO feeds
                (title, url, auth_type, auth_username, auth_password, auth_bearer_token,
                 min_fetch_interval_seconds)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             RETURNING id",
        )?;
        stmt.query_row(
            rusqlite::params![
                title,
                url,
                auth.auth_type.as_db(),
                auth.username,
                auth.password,
                auth.bearer_token,
                fetch_interval,
            ],
            |row| Ok(AddFeedQueryResult(row.get(0)?)),
        )
        .map(|r| r.0)
    })
    .await;

    let id = match task_result {
        Ok(Ok(id)) => id,
        Ok(Err(e)) => {
            event!(
                Level::ERROR,
                "failure while adding new feed to database: {:?}",
                e
            );
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response();
        }
        Err(e) => {
            event!(
                Level::ERROR,
                "error waiting for blocking thread to run SQL query: {:?}",
                e
            );
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response();
        }
    };

    event!(Level::INFO, "created new feed");
    let result = AddFeedResponse { id };

    if let Some(runner) = state.script_runner.current() {
        runner.dispatch_observe(
            crate::scripting::Event::FeedAdded,
            crate::scripting::EventPayload::Feed {
                id,
                url: url_for_event,
                title: title_for_event,
            },
        );
    }

    // Issue a command to the feed-fetch workers to make them fetch
    // the latest version of the feed.
    if let Err(e) = state
        .task_manager_tx
        .send(TaskManagerCommand::RefreshFeed(id))
        .await
    {
        event!(
            Level::ERROR,
            "failed to send fetch command for feed {}: {:?}",
            id,
            e
        );
    }

    (StatusCode::CREATED, Json(result)).into_response()
}
