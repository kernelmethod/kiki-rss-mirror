use crate::db::favicons::favicon_hash_sql;
use crate::http::FeedAuthType;
use crate::routes::v1::assets::read_asset_url_column;
use crate::routes::v1::entries::list_entries::{NOT_HIDDEN, UNREAD};
use crate::routes::v1::feeds::format_data::{
    load_atom_feed_data, load_rss_feed_data, AtomFeedData, RssFeedData,
};
use crate::server::AppState;
use crate::tasks::FetchError;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use tokio::task;
use tracing::{event, Level};

#[derive(serde::Deserialize, serde::Serialize, utoipa::ToSchema)]
pub struct GetFeedResponse {
    pub id: i64,
    pub title: String,
    pub url: String,
    pub description: Option<String>,
    /// Last checked time in RFC3339 format.
    pub last_checked: Option<String>,
    /// Minimum interval, in seconds, between fetches of this feed.
    pub min_fetch_interval_seconds: i64,
    /// Current authentication scheme for this feed. Credentials themselves
    /// are never returned — only the scheme in use.
    pub auth_type: FeedAuthType,
    /// Number of entries stored for this feed, not counting hidden ones.
    pub entry_count: i64,
    /// Number of entries stored for this feed that have not been read,
    /// not counting hidden ones.
    #[serde(default)]
    pub unread_count: i64,
    /// The website the feed belongs to, as the feed itself gives it.
    #[serde(default)]
    pub site_url: Option<String>,
    /// Relative Kiki URL that serves the favicon of the website the feed
    /// belongs to, or `null` if it has not been cached.
    #[serde(default)]
    pub favicon_url: Option<String>,
}

/// Read the `auth_type` column at `idx` and decode it into a [`FeedAuthType`].
///
/// Unrecognized values fall back to [`FeedAuthType::None`] so a bad row
/// doesn't take down an otherwise-working endpoint.
pub(crate) fn read_auth_type_column(
    row: &rusqlite::Row,
    idx: usize,
) -> rusqlite::Result<FeedAuthType> {
    let raw: Option<String> = row.get(idx)?;
    Ok(FeedAuthType::from_db(raw.as_deref()).unwrap_or_default())
}

/// Detail response for a single feed. Extends [`GetFeedResponse`] with
/// fields that are only returned by the single-feed endpoint
/// (`GET /v1/feeds/id/{id}`) — the list endpoints still return
/// `GetFeedResponse` alone.
#[derive(serde::Deserialize, serde::Serialize, utoipa::ToSchema)]
pub struct GetFeedDetailResponse {
    #[serde(flatten)]
    pub feed: GetFeedResponse,
    /// Most recent fetch error, if any. Omitted when the most recent fetch
    /// succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_fetch_error: Option<FetchError>,
    /// Time of the most recent fetch error in RFC3339 format. Omitted when
    /// the most recent fetch succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_fetch_error_at: Option<String>,
    /// RSS-specific feed-level data. Present only when the feed is ingested
    /// as RSS.
    pub rss: Option<RssFeedData>,
    /// Atom-specific feed-level data (language, rights, generator, logo,
    /// icon, authors, contributors, categories). Present only when the feed
    /// is ingested as Atom.
    pub atom: Option<AtomFeedData>,
}

/// Get feed information
///
/// Retrieve information about a single feed by that feed's ID.
#[utoipa::path(
    get,
    path = "/v1/feeds/id/{id}",
    params(
        ("id" = i64, Path, description = "Feed ID"),
    ),
    responses(
        (status = 200, description = "Feed found", body = GetFeedDetailResponse),
        (status = 404, description = "Feed not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "feeds"
)]
#[axum::debug_handler]
pub async fn get_feed(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let conn = match state.conn_pool.get() {
        Ok(conn) => conn,
        Err(e) => {
            event!(Level::ERROR, "failed to get database connection: {:?}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response();
        }
    };

    // The rusqlite interface is synchronous so we must run the INSERT
    // statement on a blocking thread.
    let task_result = task::spawn_blocking(move || {
        let mut stmt = match conn.prepare(&format!(
            "SELECT id, title, url, description, last_checked, last_fetch_error, last_fetch_error_at, min_fetch_interval_seconds, auth_type,
                (SELECT COUNT(*) FROM entries e WHERE e.feed_id = feeds.id AND {NOT_HIDDEN}),
                site_url, {},
                (SELECT COUNT(*) FROM entries e WHERE e.feed_id = feeds.id AND {UNREAD})
                FROM feeds WHERE id = ?1 LIMIT 1",
            favicon_hash_sql("feeds.id")
        )) {
            Ok(s) => s,
            Err(e) => {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
                return (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response();
            }
        };
        let query_result = stmt.query_row([id], |row| {
            let auth_type = read_auth_type_column(row, 8)?;
            let feed = GetFeedResponse {
                id: row.get(0)?,
                title: row.get(1)?,
                url: row.get(2)?,
                description: row.get(3)?,
                last_checked: row.get::<usize, Option<i64>>(4)?.and_then(|ts| {
                    chrono::DateTime::from_timestamp_secs(ts).map(|d| d.to_rfc3339())
                }),
                min_fetch_interval_seconds: row.get(7)?,
                auth_type,
                entry_count: row.get(9)?,
                site_url: row.get(10)?,
                favicon_url: read_asset_url_column(row, 11)?,
                unread_count: row.get(12)?,
            };
            let last_fetch_error = row
                .get::<usize, Option<String>>(5)?
                .and_then(|s| serde_json::from_str(&s).ok());
            let last_fetch_error_at = row.get::<usize, Option<i64>>(6)?.and_then(|ts| {
                chrono::DateTime::from_timestamp_secs(ts).map(|d| d.to_rfc3339())
            });
            Ok((feed, last_fetch_error, last_fetch_error_at))
        });
        let (feed, last_fetch_error, last_fetch_error_at) = match query_result {
            Ok(f) => f,
            Err(_e) => return (StatusCode::NOT_FOUND, "Feed not found").into_response(),
        };
        drop(stmt);

        let rss = match load_rss_feed_data(&conn, feed.id) {
            Ok(r) => r,
            Err(e) => {
                event!(Level::ERROR, "failed to load rss_feed_data: {:?}", e);
                return (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response();
            }
        };
        let atom = match load_atom_feed_data(&conn, feed.id) {
            Ok(a) => a,
            Err(e) => {
                event!(Level::ERROR, "failed to load atom_feed_data: {:?}", e);
                return (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response();
            }
        };

        let detail = GetFeedDetailResponse {
            feed,
            last_fetch_error,
            last_fetch_error_at,
            rss,
            atom,
        };
        (StatusCode::OK, Json(detail)).into_response()
    })
    .await;

    match task_result {
        Ok(res) => res,
        Err(e) => {
            event!(
                Level::ERROR,
                "error waiting for blocking thread to run SQL query: {:?}",
                e
            );
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
        }
    }
}
