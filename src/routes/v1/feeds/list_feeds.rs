use crate::routes::v1::feeds::get_feed;
use crate::server::AppState;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use tokio::task;
use tracing::{event, Level};

const DEFAULT_LIMIT: usize = 50;

#[derive(serde::Deserialize)]
pub struct ListFeedsQueryParams {
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

#[derive(serde::Deserialize, serde::Serialize)]
pub struct ListFeedsError {
    pub message: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
pub struct ListFeedsResponse {
    pub feeds: Vec<get_feed::GetFeedResponse>,
    pub count: usize,
    pub offset: usize,
    pub limit: usize,
}

impl Default for ListFeedsError {
    fn default() -> Self {
        ListFeedsError {
            message: "internal error".to_string(),
        }
    }
}

/// Route handler for listing all of the available feeds.
#[axum::debug_handler]
pub async fn list_feeds(
    State(state): State<AppState>,
    Query(params): Query<ListFeedsQueryParams>,
) -> Result<Response, Response> {
    let conn = state.conn_pool.get().unwrap();
    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT);

    let result = task::spawn_blocking(move || {
        let count = conn
            .prepare("SELECT COUNT(*) FROM feeds")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row([], |count| count.get(0))?;

        let feeds = conn
            .prepare(
                "SELECT id, title, url, description, last_checked
                FROM feeds LIMIT ?1 OFFSET ?2",
            )
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_map([limit, offset], |row| {
                Ok(get_feed::GetFeedResponse {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    url: row.get(2)?,
                    description: row.get(3)?,
                    last_checked: row.get::<usize, Option<i64>>(4)?.map(|ts| {
                        chrono::DateTime::from_timestamp_secs(ts)
                            .unwrap()
                            .to_rfc3339()
                    }),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok::<ListFeedsResponse, rusqlite::Error>(ListFeedsResponse {
            count,
            offset,
            limit,
            feeds,
        })
    })
    .await;

    match result {
        Ok(Ok(response)) => Ok(Json(response).into_response()),
        Ok(Err(e)) => {
            event!(Level::ERROR, "error in list_feeds: {:?}", e);
            let error = ListFeedsError::default();
            Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)).into_response())
        }
        Err(e) => {
            event!(Level::ERROR, "task error in list_feeds: {:?}", e);
            let error = ListFeedsError::default();
            Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)).into_response())
        }
    }
}
