use crate::routes::v1::entries::format_data::{
    load_atom_entry_data, load_rss_entry_data, AtomEntryData, RssEntryData,
};
use crate::routes::v1::entries::rows::{attach_tags, entry_columns, entry_from_row};
use crate::routes::v1::tags::list_tags::TagResponse;
use crate::server::AppState;
use axum::{
    extract::{Path, State},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use tracing::{event, Level};

#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
pub struct GetEntryResponse {
    pub id: i64,
    pub feed_id: Option<i64>,
    pub source_id: Option<i64>,
    /// Syndication format: "rss" or "atom".
    pub syndication_format: String,
    pub guid: String,
    /// Publication time in RFC3339 format.
    pub published_at: Option<String>,
    pub title: String,
    pub url: String,
    pub content: Option<String>,
    /// When Kiki first stored the entry, in RFC3339 format.
    #[serde(default)]
    pub ingested_at: Option<String>,
    /// Relative Kiki URL that serves the favicon of the website the
    /// entry's feed belongs to, or `null` if it has not been cached.
    #[serde(default)]
    pub feed_favicon_url: Option<String>,
    /// Every tag applied to the entry, both user tags and system tags (such
    /// as `system:read` and `system:saved`).
    #[serde(default)]
    pub tags: Vec<TagResponse>,
    /// RSS-specific fields (description, author, enclosure, categories).
    /// Present only for entries ingested from an RSS feed.
    pub rss: Option<RssEntryData>,
    /// Atom-specific fields (rights, authors, contributors, categories).
    /// Present only for entries ingested from an Atom feed.
    pub atom: Option<AtomEntryData>,
}

/// Get entry content
///
/// Retrieve content and metadata for a single entry by its ID.
#[utoipa::path(
    get,
    path = "/v1/entries/id/{id}",
    params(
        ("id" = i64, Path, description = "Entry ID"),
    ),
    responses(
        (status = 200, description = "Entry found", body = GetEntryResponse),
        (status = 404, description = "Entry not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "entries"
)]
pub async fn get_entry(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, Response> {
    let result = state
        .db
        .read(move |conn| {
            let entry = conn
                .prepare(&format!(
                    "SELECT {} FROM entries e WHERE e.id = ?1 LIMIT 1",
                    entry_columns()
                ))
                .inspect_err(|e| {
                    event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
                })?
                .query_row([id], entry_from_row)
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    _ => Err(e),
                })
                .inspect_err(|e| {
                    event!(Level::ERROR, "failed to get entry: {:?}", e);
                })?;

            let entry = match entry {
                Some(e) => {
                    let mut entries = [e];
                    attach_tags(conn, &mut entries)?;
                    let [e] = entries;
                    let rss = load_rss_entry_data(conn, e.id).inspect_err(|err| {
                        event!(Level::ERROR, "failed to load rss_entry_data: {:?}", err);
                    })?;
                    let atom = load_atom_entry_data(conn, e.id).inspect_err(|err| {
                        event!(Level::ERROR, "failed to load atom_entry_data: {:?}", err);
                    })?;
                    Some(GetEntryResponse {
                        id: e.id,
                        feed_id: e.feed_id,
                        source_id: e.source_id,
                        syndication_format: e.syndication_format,
                        guid: e.guid,
                        published_at: e.published_at,
                        title: e.title,
                        url: e.url,
                        content: e.content,
                        ingested_at: e.ingested_at,
                        feed_favicon_url: e.feed_favicon_url,
                        tags: e.tags,
                        rss,
                        atom,
                    })
                }
                None => None,
            };

            Ok::<Option<GetEntryResponse>, rusqlite::Error>(entry)
        })
        .await;

    match result {
        Ok(Ok(Some(entry))) => Ok((axum::http::StatusCode::OK, Json(entry)).into_response()),
        Ok(Ok(None)) => Err((axum::http::StatusCode::NOT_FOUND, "Entry not found").into_response()),
        Ok(Err(e)) => {
            event!(Level::ERROR, "error in get_entry: {:?}", e);
            Err((
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error",
            )
                .into_response())
        }
        Err(e) => {
            event!(Level::ERROR, "task error in get_entry: {:?}", e);
            Err((
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error",
            )
                .into_response())
        }
    }
}
