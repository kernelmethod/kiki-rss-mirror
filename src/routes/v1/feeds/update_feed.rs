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
use tracing::{event, Level};

#[derive(Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct UpdateFeedRequest {
    pub title: Option<String>,
    pub url: Option<String>,
    pub description: Option<String>,
    /// Minimum interval, in seconds, between fetches of this feed.
    pub min_fetch_interval_seconds: Option<i64>,
    /// Authentication scheme to apply when fetching this feed. Setting
    /// `"none"` clears any existing credentials.
    pub auth_type: Option<FeedAuthType>,
    /// Username for HTTP Basic auth. Omitted fields are left unchanged. When `auth_type`
    /// is omitted, this may only be set if the feed already uses `"basic"`.
    pub auth_username: Option<String>,
    /// Password for HTTP Basic auth. Omitted fields are left unchanged. When `auth_type`
    /// is omitted, this may only be set if the feed already uses `"basic"`.
    pub auth_password: Option<String>,
    /// Token for HTTP Bearer auth. Omitted fields are left unchanged. When `auth_type`
    /// is omitted, this may only be set if the feed already uses `"bearer"`.
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

    #[error(
        "'{field}' cannot be set while auth_type is '{auth_type}'; set auth_type to change schemes"
    )]
    CredentialSchemeMismatch {
        field: &'static str,
        auth_type: FeedAuthType,
    },

    #[error("a feed with this URL already exists")]
    DuplicateUrl,

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
}

/// The columns of a `feeds` row that decide how and how often it is
/// fetched, compared before and after an update to see whether the feed
/// needs rescheduling.
#[derive(PartialEq, Eq)]
struct FetchConfig {
    url: String,
    auth: (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ),
    min_fetch_interval_seconds: i64,
}

fn read_fetch_config(conn: &rusqlite::Connection, id: i64) -> rusqlite::Result<FetchConfig> {
    conn.query_row(
        "SELECT url, auth_type, auth_username, auth_password, auth_bearer_token,
            min_fetch_interval_seconds
         FROM feeds WHERE id = ?1",
        [id],
        |row| {
            Ok(FetchConfig {
                url: row.get(0)?,
                auth: (row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?),
                min_fetch_interval_seconds: row.get(5)?,
            })
        },
    )
}

/// Reschedule feed `id` so an edit to how it is fetched takes effect now,
/// rather than after the fetch that was already scheduled.
///
/// - A new URL or new credentials may well fix a feed that has been
///   failing, so the feed is made due at once and its failure streak and
///   `Retry-After` are forgotten.
/// - A shorter interval pulls the next fetch in to one interval after the
///   last successful check. A feed backing off after errors keeps its
///   backoff, and a longer interval takes effect after the next fetch.
fn reschedule_after_update(
    conn: &rusqlite::Connection,
    id: i64,
    before: &FetchConfig,
    after: &FetchConfig,
) -> rusqlite::Result<()> {
    if before.url != after.url || before.auth != after.auth {
        conn.execute(
            "UPDATE feeds
             SET next_fetch_at = NULL, consecutive_failures = 0, retry_after_at = NULL
             WHERE id = ?1",
            [id],
        )?;
        return Ok(());
    }
    if after.min_fetch_interval_seconds < before.min_fetch_interval_seconds {
        // MIN() with a NULL `next_fetch_at` stays NULL, i.e. already due.
        conn.execute(
            "UPDATE feeds
             SET next_fetch_at = MIN(next_fetch_at, COALESCE(last_checked, 0) + ?2)
             WHERE id = ?1 AND consecutive_failures = 0",
            [id, after.min_fetch_interval_seconds],
        )?;
    }
    Ok(())
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
        (status = 400, description = "No update fields provided, or invalid field values"),
        (status = 404, description = "Feed not found"),
        (status = 409, description = "Another feed already has the new URL"),
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
    let result = state
        .db
        .write(move |conn| {
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
            } else if payload.auth_username.is_some()
                || payload.auth_password.is_some()
                || payload.auth_bearer_token.is_some()
            {
                // Patching individual credentials is only allowed for fields used
                // by the feed's current scheme; otherwise we'd store secrets that
                // are never sent and never cleared.
                let current = conn
                    .prepare(
                        "SELECT auth_type, auth_username, auth_password, auth_bearer_token
                    FROM feeds WHERE id = ?1",
                    )
                    .inspect_err(|e| {
                        event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
                    })?
                    .query_row([id], |row| {
                        let auth_type_raw: Option<String> = row.get(0)?;
                        Ok(FeedAuth {
                            auth_type: FeedAuthType::from_db(auth_type_raw.as_deref())
                                .unwrap_or_default(),
                            username: row.get(1)?,
                            password: row.get(2)?,
                            bearer_token: row.get(3)?,
                        })
                    })?;

                let allowed: &[&'static str] = match current.auth_type {
                    FeedAuthType::None => &[],
                    FeedAuthType::Basic => &["auth_username", "auth_password"],
                    FeedAuthType::Bearer => &["auth_bearer_token"],
                };
                let provided = [
                    ("auth_username", payload.auth_username.is_some()),
                    ("auth_password", payload.auth_password.is_some()),
                    ("auth_bearer_token", payload.auth_bearer_token.is_some()),
                ];
                if let Some((field, _)) = provided
                    .iter()
                    .find(|(field, set)| *set && !allowed.contains(field))
                {
                    return Err(UpdateFeedTaskError::CredentialSchemeMismatch {
                        field,
                        auth_type: current.auth_type,
                    });
                }

                // Make sure the patched credentials still form a valid
                // configuration (e.g. a basic username isn't blanked out).
                let patched = FeedAuth {
                    auth_type: current.auth_type,
                    username: payload.auth_username.clone().or(current.username),
                    password: payload.auth_password.clone().or(current.password),
                    bearer_token: payload.auth_bearer_token.clone().or(current.bearer_token),
                };
                patched.validate()?;

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

            // Execute the update, and bring the feed's schedule in line with it,
            // in one transaction. It takes the write lock up front: a deferred
            // transaction that reads and then writes fails at once with
            // SQLITE_BUSY (no busy_timeout wait) if a worker commits a write in
            // between, e.g. the refresh queued when the feed was added.
            let tx = rusqlite::Transaction::new_unchecked(
                conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            let before = read_fetch_config(&tx, id)?;
            let params = rusqlite::params_from_iter(params);
            match tx.execute(&query, params) {
                Ok(_) => {}
                Err(rusqlite::Error::SqliteFailure(err, _))
                    if err.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    return Err(UpdateFeedTaskError::DuplicateUrl);
                }
                Err(e) => {
                    event!(Level::ERROR, "unable to execute update statement: {:?}", e);
                    return Err(e.into());
                }
            }
            let after = read_fetch_config(&tx, id)?;
            reschedule_after_update(&tx, id, &before, &after)?;
            tx.commit()?;

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
                    let auth_type =
                        FeedAuthType::from_db(auth_type_raw.as_deref()).unwrap_or_default();
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
        Ok(Err(e @ UpdateFeedTaskError::CredentialSchemeMismatch { .. })) => {
            Err((StatusCode::BAD_REQUEST, format!("{e}")).into_response())
        }
        Ok(Err(UpdateFeedTaskError::InvalidAuth(e))) => {
            Err((StatusCode::BAD_REQUEST, format!("{e}")).into_response())
        }
        Ok(Err(UpdateFeedTaskError::DuplicateUrl)) => {
            Err((StatusCode::CONFLICT, "A feed with this URL already exists").into_response())
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

#[cfg(test)]
mod test {
    use super::*;
    use crate::routes::v1::feeds::add_feed::{AddFeedRequest, AddFeedResponse};
    use crate::routes::v1::feeds::get_feed::GetFeedResponse;
    use crate::test::{TestBuilder, TestConfig};
    use anyhow::Result;

    /// Columns of a `feeds` row that `update_feed` is able to modify.
    #[derive(Debug, PartialEq)]
    struct FeedRow {
        title: String,
        url: Option<String>,
        description: Option<String>,
        min_fetch_interval_seconds: i64,
        auth_type: Option<String>,
        auth_username: Option<String>,
        auth_password: Option<String>,
        auth_bearer_token: Option<String>,
    }

    fn read_feed_row(tc: &TestConfig, id: i64) -> Result<FeedRow> {
        let conn = tc.database_conn()?;
        let row = conn.query_row(
            "SELECT title, url, description, min_fetch_interval_seconds,
                    auth_type, auth_username, auth_password, auth_bearer_token
             FROM feeds WHERE id = ?1",
            [id],
            |row| {
                Ok(FeedRow {
                    title: row.get(0)?,
                    url: row.get(1)?,
                    description: row.get(2)?,
                    min_fetch_interval_seconds: row.get(3)?,
                    auth_type: row.get(4)?,
                    auth_username: row.get(5)?,
                    auth_password: row.get(6)?,
                    auth_bearer_token: row.get(7)?,
                })
            },
        )?;
        Ok(row)
    }

    async fn create_feed(client: &reqwest::Client, req: AddFeedRequest) -> Result<i64> {
        let resp = client
            .post("http://localhost/v1/feeds/create")
            .json(&req)
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        Ok(resp.json::<AddFeedResponse>().await?.id)
    }

    async fn create_plain_feed(client: &reqwest::Client) -> Result<i64> {
        create_feed(
            client,
            AddFeedRequest {
                title: "original title".into(),
                url: "https://example.com/original.xml".into(),
                ..Default::default()
            },
        )
        .await
    }

    async fn put_feed(
        client: &reqwest::Client,
        id: i64,
        req: &UpdateFeedRequest,
    ) -> Result<reqwest::Response> {
        Ok(client
            .put(format!("http://localhost/v1/feeds/id/{id}"))
            .json(req)
            .send()
            .await?)
    }

    #[tokio::test]
    async fn test_update_feed_url() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        let id = create_plain_feed(&client).await?;

        let resp = put_feed(
            &client,
            id,
            &UpdateFeedRequest {
                url: Some("https://example.com/moved.xml".into()),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<UpdateFeedResponse>().await?;
        assert_eq!(body.id, id);
        assert_eq!(body.url, "https://example.com/moved.xml");
        assert_eq!(body.title, "original title");

        let resp = client
            .get(format!("http://localhost/v1/feeds/id/{id}"))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let feed = resp.json::<GetFeedResponse>().await?;
        assert_eq!(feed.url, "https://example.com/moved.xml");

        Ok(())
    }

    /// Adding a feed with the URL of an existing one fails with `409
    /// Conflict`, and so does moving a feed to another feed's URL.
    #[tokio::test]
    async fn test_feed_urls_are_unique() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        let id = create_plain_feed(&client).await?;

        let resp = client
            .post("http://localhost/v1/feeds/create")
            .json(&AddFeedRequest {
                title: "copy".into(),
                url: "https://example.com/original.xml".into(),
                ..Default::default()
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CONFLICT);

        let other = create_feed(
            &client,
            AddFeedRequest {
                title: "other".into(),
                url: "https://example.com/other.xml".into(),
                ..Default::default()
            },
        )
        .await?;
        let before = read_feed_row(&tc, other)?;
        let resp = put_feed(
            &client,
            other,
            &UpdateFeedRequest {
                title: Some("renamed".into()),
                url: Some("https://example.com/original.xml".into()),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        assert_eq!(read_feed_row(&tc, other)?, before);
        assert_eq!(
            read_feed_row(&tc, id)?.url.as_deref(),
            Some("https://example.com/original.xml")
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_update_feed_partial_update_preserves_other_fields() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        let id = create_plain_feed(&client).await?;

        // Set a description and interval first so there is something to preserve.
        let resp = put_feed(
            &client,
            id,
            &UpdateFeedRequest {
                description: Some("a description".into()),
                min_fetch_interval_seconds: Some(600),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let before = read_feed_row(&tc, id)?;

        // Updating only the title must leave every other column untouched.
        let resp = put_feed(
            &client,
            id,
            &UpdateFeedRequest {
                title: Some("new title".into()),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<UpdateFeedResponse>().await?;
        assert_eq!(body.title, "new title");
        assert_eq!(body.url, "https://example.com/original.xml");
        assert_eq!(body.description.as_deref(), Some("a description"));
        assert_eq!(body.min_fetch_interval_seconds, 600);
        assert_eq!(body.auth_type, FeedAuthType::None);

        let after = read_feed_row(&tc, id)?;
        assert_eq!(
            after,
            FeedRow {
                title: "new title".into(),
                ..before
            }
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_update_feed_default_fetch_interval_reported() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        let id = create_plain_feed(&client).await?;

        let resp = put_feed(
            &client,
            id,
            &UpdateFeedRequest {
                description: Some("d".into()),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<UpdateFeedResponse>().await?;
        assert_eq!(
            body.min_fetch_interval_seconds as u64,
            crate::config::DEFAULT_FETCH_INTERVAL_SECONDS
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_update_feed_rejects_non_positive_fetch_interval() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        let id = create_plain_feed(&client).await?;
        let before = read_feed_row(&tc, id)?;

        for interval in [0, -1, i64::MIN] {
            // Pair the invalid interval with a valid title change to make sure
            // the request is rejected as a whole rather than partially applied.
            let resp = put_feed(
                &client,
                id,
                &UpdateFeedRequest {
                    title: Some("should not be applied".into()),
                    min_fetch_interval_seconds: Some(interval),
                    ..Default::default()
                },
            )
            .await?;
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "interval {interval}"
            );
            assert_eq!(
                resp.text().await?,
                "min_fetch_interval_seconds must be positive"
            );
        }

        assert_eq!(read_feed_row(&tc, id)?, before);

        // The smallest positive interval is accepted.
        let resp = put_feed(
            &client,
            id,
            &UpdateFeedRequest {
                min_fetch_interval_seconds: Some(1),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<UpdateFeedResponse>().await?;
        assert_eq!(body.min_fetch_interval_seconds, 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_update_feed_empty_body() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        let id = create_plain_feed(&client).await?;

        let resp = client
            .put(format!("http://localhost/v1/feeds/id/{id}"))
            .json(&serde_json::json!({}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(resp.text().await?, "Invalid update parameters");

        Ok(())
    }

    #[tokio::test]
    async fn test_update_feed_not_found_checked_before_validation() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        // A request that would otherwise fail validation should still report
        // 404 when the feed doesn't exist.
        let requests = [
            UpdateFeedRequest::default(),
            UpdateFeedRequest {
                min_fetch_interval_seconds: Some(0),
                ..Default::default()
            },
            UpdateFeedRequest {
                auth_type: Some(FeedAuthType::Bearer),
                ..Default::default()
            },
        ];
        for req in &requests {
            let resp = put_feed(&client, 12345, req).await?;
            assert_eq!(resp.status(), StatusCode::NOT_FOUND);
            assert_eq!(resp.text().await?, "Feed not found");
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_update_feed_malformed_requests() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        let id = create_plain_feed(&client).await?;

        // Non-numeric feed ID
        let resp = client
            .put("http://localhost/v1/feeds/id/not-a-number")
            .json(&UpdateFeedRequest {
                title: Some("x".into()),
                ..Default::default()
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // Unknown auth scheme
        let resp = client
            .put(format!("http://localhost/v1/feeds/id/{id}"))
            .json(&serde_json::json!({ "auth_type": "digest" }))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

        // Wrong type for min_fetch_interval_seconds
        let resp = client
            .put(format!("http://localhost/v1/feeds/id/{id}"))
            .json(&serde_json::json!({ "min_fetch_interval_seconds": "soon" }))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

        // Body that isn't JSON at all
        let resp = client
            .put(format!("http://localhost/v1/feeds/id/{id}"))
            .header("content-type", "application/json")
            .body("{not json")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // Missing JSON content type
        let resp = client
            .put(format!("http://localhost/v1/feeds/id/{id}"))
            .body(r#"{"title": "x"}"#)
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

        assert_eq!(read_feed_row(&tc, id)?.title, "original title");

        Ok(())
    }

    #[tokio::test]
    async fn test_update_feed_set_basic_auth() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        // Start from bearer so we can verify the token is cleared.
        let id = create_feed(
            &client,
            AddFeedRequest {
                title: "private".into(),
                url: "https://example.com/private.xml".into(),
                auth_type: Some(FeedAuthType::Bearer),
                auth_bearer_token: Some("tok".into()),
                ..Default::default()
            },
        )
        .await?;

        let resp = put_feed(
            &client,
            id,
            &UpdateFeedRequest {
                auth_type: Some(FeedAuthType::Basic),
                auth_username: Some("alice".into()),
                auth_password: Some("hunter2".into()),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::OK);

        // Credentials must never be echoed back to the client.
        let body: serde_json::Value = resp.json().await?;
        assert_eq!(body.get("auth_type"), Some(&serde_json::json!("basic")));
        assert!(body.get("auth_username").is_none());
        assert!(body.get("auth_password").is_none());
        assert!(body.get("auth_bearer_token").is_none());

        let row = read_feed_row(&tc, id)?;
        assert_eq!(row.auth_type.as_deref(), Some("basic"));
        assert_eq!(row.auth_username.as_deref(), Some("alice"));
        assert_eq!(row.auth_password.as_deref(), Some("hunter2"));
        assert_eq!(
            row.auth_bearer_token, None,
            "bearer token should be cleared"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_update_feed_basic_auth_without_password() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        let id = create_plain_feed(&client).await?;

        let resp = put_feed(
            &client,
            id,
            &UpdateFeedRequest {
                auth_type: Some(FeedAuthType::Basic),
                auth_username: Some("alice".into()),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::OK);

        let row = read_feed_row(&tc, id)?;
        assert_eq!(row.auth_type.as_deref(), Some("basic"));
        assert_eq!(row.auth_username.as_deref(), Some("alice"));
        assert_eq!(row.auth_password, None);

        Ok(())
    }

    #[tokio::test]
    async fn test_update_feed_rejects_invalid_auth() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        let id = create_feed(
            &client,
            AddFeedRequest {
                title: "private".into(),
                url: "https://example.com/private.xml".into(),
                auth_type: Some(FeedAuthType::Basic),
                auth_username: Some("alice".into()),
                auth_password: Some("hunter2".into()),
                ..Default::default()
            },
        )
        .await?;
        let before = read_feed_row(&tc, id)?;

        let cases = [
            (
                UpdateFeedRequest {
                    auth_type: Some(FeedAuthType::Basic),
                    auth_password: Some("p".into()),
                    ..Default::default()
                },
                "auth_type 'basic' requires 'auth_username'",
            ),
            (
                UpdateFeedRequest {
                    auth_type: Some(FeedAuthType::Basic),
                    auth_username: Some(String::new()),
                    ..Default::default()
                },
                "auth_type 'basic' requires 'auth_username'",
            ),
            (
                UpdateFeedRequest {
                    auth_type: Some(FeedAuthType::Bearer),
                    ..Default::default()
                },
                "auth_type 'bearer' requires 'auth_bearer_token'",
            ),
            (
                UpdateFeedRequest {
                    // Also includes a valid field that must not be applied.
                    title: Some("should not be applied".into()),
                    auth_type: Some(FeedAuthType::Bearer),
                    auth_bearer_token: Some(String::new()),
                    ..Default::default()
                },
                "auth_type 'bearer' requires 'auth_bearer_token'",
            ),
        ];

        for (req, expected) in &cases {
            let resp = put_feed(&client, id, req).await?;
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            assert_eq!(&resp.text().await?, expected);
        }

        // None of the rejected requests should have modified the feed.
        assert_eq!(read_feed_row(&tc, id)?, before);

        Ok(())
    }

    #[tokio::test]
    async fn test_update_feed_auth_none_ignores_supplied_credentials() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        let id = create_feed(
            &client,
            AddFeedRequest {
                title: "private".into(),
                url: "https://example.com/private.xml".into(),
                auth_type: Some(FeedAuthType::Basic),
                auth_username: Some("alice".into()),
                auth_password: Some("hunter2".into()),
                ..Default::default()
            },
        )
        .await?;

        let resp = put_feed(
            &client,
            id,
            &UpdateFeedRequest {
                auth_type: Some(FeedAuthType::None),
                auth_username: Some("bob".into()),
                auth_password: Some("pw".into()),
                auth_bearer_token: Some("tok".into()),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<UpdateFeedResponse>().await?;
        assert_eq!(body.auth_type, FeedAuthType::None);

        let row = read_feed_row(&tc, id)?;
        assert_eq!(row.auth_type, None);
        assert_eq!(row.auth_username, None);
        assert_eq!(row.auth_password, None);
        assert_eq!(row.auth_bearer_token, None);

        Ok(())
    }

    #[tokio::test]
    async fn test_update_feed_patch_credentials_without_auth_type() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        let id = create_feed(
            &client,
            AddFeedRequest {
                title: "private".into(),
                url: "https://example.com/private.xml".into(),
                auth_type: Some(FeedAuthType::Basic),
                auth_username: Some("alice".into()),
                auth_password: Some("hunter2".into()),
                ..Default::default()
            },
        )
        .await?;

        // Rotating just the password leaves the scheme and username alone.
        let resp = put_feed(
            &client,
            id,
            &UpdateFeedRequest {
                auth_password: Some("correct horse".into()),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<UpdateFeedResponse>().await?;
        assert_eq!(body.auth_type, FeedAuthType::Basic);

        let row = read_feed_row(&tc, id)?;
        assert_eq!(row.auth_type.as_deref(), Some("basic"));
        assert_eq!(row.auth_username.as_deref(), Some("alice"));
        assert_eq!(row.auth_password.as_deref(), Some("correct horse"));
        assert_eq!(row.auth_bearer_token, None);

        // Patching the username alone works the same way.
        let resp = put_feed(
            &client,
            id,
            &UpdateFeedRequest {
                auth_username: Some("carol".into()),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let row = read_feed_row(&tc, id)?;
        assert_eq!(row.auth_username.as_deref(), Some("carol"));
        assert_eq!(row.auth_password.as_deref(), Some("correct horse"));

        // A bearer token belongs to a different scheme, so it can't be patched
        // in without also switching auth_type.
        let before = read_feed_row(&tc, id)?;
        let resp = put_feed(
            &client,
            id,
            &UpdateFeedRequest {
                auth_bearer_token: Some("tok".into()),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            resp.text().await?,
            "'auth_bearer_token' cannot be set while auth_type is 'basic'; \
             set auth_type to change schemes"
        );
        assert_eq!(read_feed_row(&tc, id)?, before);

        Ok(())
    }

    #[tokio::test]
    async fn test_update_feed_rejects_credentials_for_other_scheme() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        // A feed without auth accepts no credential fields at all.
        let plain = create_plain_feed(&client).await?;
        let before = read_feed_row(&tc, plain)?;
        for (req, field) in [
            (
                UpdateFeedRequest {
                    auth_username: Some("alice".into()),
                    ..Default::default()
                },
                "auth_username",
            ),
            (
                UpdateFeedRequest {
                    auth_password: Some("pw".into()),
                    ..Default::default()
                },
                "auth_password",
            ),
            (
                UpdateFeedRequest {
                    // Also includes a valid field that must not be applied.
                    title: Some("should not be applied".into()),
                    auth_bearer_token: Some("tok".into()),
                    ..Default::default()
                },
                "auth_bearer_token",
            ),
        ] {
            let resp = put_feed(&client, plain, &req).await?;
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            assert_eq!(
                resp.text().await?,
                format!(
                    "'{field}' cannot be set while auth_type is 'none'; \
                     set auth_type to change schemes"
                )
            );
        }
        assert_eq!(read_feed_row(&tc, plain)?, before);

        // A bearer feed rejects basic credentials but accepts a new token.
        let bearer = create_feed(
            &client,
            AddFeedRequest {
                title: "bearer".into(),
                url: "https://example.com/bearer.xml".into(),
                auth_type: Some(FeedAuthType::Bearer),
                auth_bearer_token: Some("old".into()),
                ..Default::default()
            },
        )
        .await?;
        let resp = put_feed(
            &client,
            bearer,
            &UpdateFeedRequest {
                auth_bearer_token: Some("new".into()),
                auth_username: Some("alice".into()),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            read_feed_row(&tc, bearer)?.auth_bearer_token.as_deref(),
            Some("old")
        );

        let resp = put_feed(
            &client,
            bearer,
            &UpdateFeedRequest {
                auth_bearer_token: Some("new".into()),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let row = read_feed_row(&tc, bearer)?;
        assert_eq!(row.auth_type.as_deref(), Some("bearer"));
        assert_eq!(row.auth_bearer_token.as_deref(), Some("new"));
        assert_eq!(row.auth_username, None);

        Ok(())
    }

    #[tokio::test]
    async fn test_update_feed_rejects_blanking_required_credential() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        let basic = create_feed(
            &client,
            AddFeedRequest {
                title: "basic".into(),
                url: "https://example.com/basic.xml".into(),
                auth_type: Some(FeedAuthType::Basic),
                auth_username: Some("alice".into()),
                ..Default::default()
            },
        )
        .await?;
        let bearer = create_feed(
            &client,
            AddFeedRequest {
                title: "bearer".into(),
                url: "https://example.com/bearer.xml".into(),
                auth_type: Some(FeedAuthType::Bearer),
                auth_bearer_token: Some("tok".into()),
                ..Default::default()
            },
        )
        .await?;

        let resp = put_feed(
            &client,
            basic,
            &UpdateFeedRequest {
                auth_username: Some(String::new()),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            resp.text().await?,
            "auth_type 'basic' requires 'auth_username'"
        );
        assert_eq!(
            read_feed_row(&tc, basic)?.auth_username.as_deref(),
            Some("alice")
        );

        let resp = put_feed(
            &client,
            bearer,
            &UpdateFeedRequest {
                auth_bearer_token: Some(String::new()),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            resp.text().await?,
            "auth_type 'bearer' requires 'auth_bearer_token'"
        );
        assert_eq!(
            read_feed_row(&tc, bearer)?.auth_bearer_token.as_deref(),
            Some("tok")
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_update_feed_combined_fields_and_auth() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        let id = create_plain_feed(&client).await?;

        let resp = put_feed(
            &client,
            id,
            &UpdateFeedRequest {
                title: Some("t".into()),
                url: Some("https://example.com/new.xml".into()),
                description: Some("d".into()),
                min_fetch_interval_seconds: Some(60),
                auth_type: Some(FeedAuthType::Bearer),
                auth_bearer_token: Some("tok".into()),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<UpdateFeedResponse>().await?;
        assert_eq!(body.id, id);
        assert_eq!(body.title, "t");
        assert_eq!(body.url, "https://example.com/new.xml");
        assert_eq!(body.description.as_deref(), Some("d"));
        assert_eq!(body.min_fetch_interval_seconds, 60);
        assert_eq!(body.auth_type, FeedAuthType::Bearer);

        assert_eq!(
            read_feed_row(&tc, id)?,
            FeedRow {
                title: "t".into(),
                url: Some("https://example.com/new.xml".into()),
                description: Some("d".into()),
                min_fetch_interval_seconds: 60,
                auth_type: Some("bearer".into()),
                auth_username: None,
                auth_password: None,
                auth_bearer_token: Some("tok".into()),
            }
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_update_feed_only_affects_target_feed() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        let id_a = create_plain_feed(&client).await?;
        let id_b = create_feed(
            &client,
            AddFeedRequest {
                title: "other".into(),
                url: "https://example.com/other.xml".into(),
                ..Default::default()
            },
        )
        .await?;
        let before_b = read_feed_row(&tc, id_b)?;

        let resp = put_feed(
            &client,
            id_a,
            &UpdateFeedRequest {
                title: Some("changed".into()),
                min_fetch_interval_seconds: Some(42),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::OK);

        assert_eq!(read_feed_row(&tc, id_a)?.title, "changed");
        assert_eq!(read_feed_row(&tc, id_b)?, before_b);

        Ok(())
    }
}
