use crate::routes::v1::entries::list_entries::{ListEntriesResponseEntry, DEFAULT_LIMIT};
use crate::server::AppState;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use rusqlite::types::ToSqlOutput;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::task;
use tracing::{event, Level};

/// A tag filter expression supporting AND/OR combinations.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(untagged)]
#[schema(no_recursion)]
pub enum TagFilter {
    /// A single tag name.
    Single(String),
    /// Boolean combination of tags.
    Expr(TagExpr),
}

/// Boolean tag expression.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(no_recursion)]
pub enum TagExpr {
    /// All of these must match (AND semantics).
    #[serde(rename = "and")]
    And(Vec<TagFilter>),
    /// At least one must match (OR semantics).
    #[serde(rename = "or")]
    Or(Vec<TagFilter>),
}

/// Request body for the entry search endpoint.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct SearchEntriesRequest {
    /// Tag filter expression. Supports AND/OR combinations.
    pub tags: Option<TagFilter>,
    /// Lower bound for published_at (inclusive), RFC3339 string.
    pub published_after: Option<String>,
    /// Upper bound for published_at (exclusive), RFC3339 string.
    pub published_before: Option<String>,
    /// GLOB pattern matched against title (case-sensitive, * and ? wildcards).
    pub title_glob: Option<String>,
    /// GLOB pattern matched against content.
    pub content_glob: Option<String>,
    /// GLOB pattern matched against URL.
    pub url_glob: Option<String>,
    /// Number of records to skip (default: 0).
    pub offset: Option<usize>,
    /// Maximum number of records to return (default: 50).
    pub limit: Option<usize>,
}

/// Response from the entry search endpoint.
#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
pub struct SearchEntriesResponse {
    pub entries: Vec<ListEntriesResponseEntry>,
    pub count: usize,
    pub offset: usize,
    pub limit: usize,
}

#[derive(Error, Debug)]
enum SearchEntriesError {
    #[error("invalid date: {0}")]
    InvalidDate(String),

    #[error("empty tag filter array")]
    EmptyTagFilter,

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
}

/// Wrapper around a value that implements `rusqlite::types::ToSql`, used to
/// build a dynamic list of query parameters.
struct SqlParam(ToSqlOutput<'static>);

impl rusqlite::types::ToSql for SqlParam {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.0.clone())
    }
}

impl SqlParam {
    fn from_i64(v: i64) -> Self {
        SqlParam(ToSqlOutput::Owned(rusqlite::types::Value::Integer(v)))
    }

    fn from_string(v: String) -> Self {
        SqlParam(ToSqlOutput::Owned(rusqlite::types::Value::Text(v)))
    }

    fn from_usize(v: usize) -> Self {
        SqlParam(ToSqlOutput::Owned(rusqlite::types::Value::Integer(
            v as i64,
        )))
    }
}

/// Recursively translate a `TagFilter` into a SQL condition fragment and
/// append the corresponding bind parameters to `params`.
///
/// `idx` is the current 1-based parameter index and returns the next free
/// index after all parameters added by this call.
fn tag_filter_to_sql(
    filter: &TagFilter,
    params: &mut Vec<SqlParam>,
    idx: usize,
) -> Result<(String, usize), SearchEntriesError> {
    match filter {
        TagFilter::Single(name) => {
            let sql = format!(
                "e.id IN (SELECT et.entry_id FROM entry_tags et \
                 JOIN tags t ON et.tag_id = t.id WHERE t.name = ?{})",
                idx
            );
            params.push(SqlParam::from_string(name.clone()));
            Ok((sql, idx + 1))
        }
        TagFilter::Expr(expr) => match expr {
            TagExpr::Or(filters) => {
                if filters.is_empty() {
                    return Err(SearchEntriesError::EmptyTagFilter);
                }
                // Collect all leaf tag names for a flat OR
                let mut names = Vec::new();
                collect_or_leaves(filters, &mut names)?;

                let placeholders: Vec<String> = (idx..idx + names.len())
                    .map(|i| format!("?{}", i))
                    .collect();
                let sql = format!(
                    "e.id IN (SELECT DISTINCT et.entry_id FROM entry_tags et \
                     JOIN tags t ON et.tag_id = t.id WHERE t.name IN ({}))",
                    placeholders.join(", ")
                );
                for name in &names {
                    params.push(SqlParam::from_string(name.clone()));
                }
                Ok((sql, idx + names.len()))
            }
            TagExpr::And(filters) => {
                if filters.is_empty() {
                    return Err(SearchEntriesError::EmptyTagFilter);
                }
                let mut conditions = Vec::new();
                let mut current_idx = idx;
                for f in filters {
                    let (cond, next_idx) = tag_filter_to_sql(f, params, current_idx)?;
                    conditions.push(cond);
                    current_idx = next_idx;
                }
                let sql = format!("({})", conditions.join(" AND "));
                Ok((sql, current_idx))
            }
        },
    }
}

/// Recursively collect leaf tag names from an OR expression.
fn collect_or_leaves(
    filters: &[TagFilter],
    out: &mut Vec<String>,
) -> Result<(), SearchEntriesError> {
    for f in filters {
        match f {
            TagFilter::Single(name) => out.push(name.clone()),
            TagFilter::Expr(TagExpr::Or(inner)) => {
                if inner.is_empty() {
                    return Err(SearchEntriesError::EmptyTagFilter);
                }
                collect_or_leaves(inner, out)?;
            }
            // Nested AND inside OR: delegate to normal recursive translation
            // (will be handled by the AND branch of tag_filter_to_sql)
            TagFilter::Expr(TagExpr::And(_)) => {
                // For nested AND inside OR, we can't flatten — fall back to
                // the general recursive approach by returning an error that
                // signals the caller to use the non-flattened path.
                // Actually, we should not hit this in collect_or_leaves
                // because the caller only calls this for Or variants.
                // For correctness, just push nothing and let the caller
                // handle it via tag_filter_to_sql directly.
                // Re-design: instead of collecting, handle mixed OR with
                // nested AND by using the general path.
                return Err(SearchEntriesError::EmptyTagFilter);
            }
        }
    }
    Ok(())
}

/// Build a tag filter SQL condition. Falls back to the general recursive
/// approach if the OR contains nested AND expressions.
fn build_tag_condition(
    filter: &TagFilter,
    params: &mut Vec<SqlParam>,
    idx: usize,
) -> Result<(String, usize), SearchEntriesError> {
    // For OR with mixed children (containing AND), use per-child approach
    if let TagFilter::Expr(TagExpr::Or(filters)) = filter {
        if filters.is_empty() {
            return Err(SearchEntriesError::EmptyTagFilter);
        }
        // Check if any child is an AND expression
        let has_nested_and = filters
            .iter()
            .any(|f| matches!(f, TagFilter::Expr(TagExpr::And(_))));
        if has_nested_and {
            let mut conditions = Vec::new();
            let mut current_idx = idx;
            for f in filters {
                let (cond, next_idx) = build_tag_condition(f, params, current_idx)?;
                conditions.push(cond);
                current_idx = next_idx;
            }
            let sql = format!("({})", conditions.join(" OR "));
            return Ok((sql, current_idx));
        }
    }
    tag_filter_to_sql(filter, params, idx)
}

fn parse_rfc3339_to_timestamp(s: &str) -> Result<i64, SearchEntriesError> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.timestamp())
        .map_err(|_| SearchEntriesError::InvalidDate(s.to_string()))
}

/// Search entries
///
/// Search for entries using tag filters, date ranges, and GLOB patterns.
/// Uses POST because the tag query requires a structured expression.
#[utoipa::path(
    post,
    path = "/v1/entries/search",
    request_body = SearchEntriesRequest,
    responses(
        (status = 200, description = "Search results", body = SearchEntriesResponse),
        (status = 400, description = "Invalid search parameters"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "entries"
)]
pub async fn search_entries(
    State(state): State<AppState>,
    Json(payload): Json<SearchEntriesRequest>,
) -> Result<Response, Response> {
    let conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;

    let offset = payload.offset.unwrap_or(0);
    let limit = payload.limit.unwrap_or(DEFAULT_LIMIT);

    let result = task::spawn_blocking(move || {
        let mut conditions: Vec<String> = Vec::new();
        let mut params: Vec<SqlParam> = Vec::new();
        let mut next_idx: usize = 1;

        // Tag filter
        if let Some(ref tag_filter) = payload.tags {
            let (cond, new_idx) = build_tag_condition(tag_filter, &mut params, next_idx)?;
            conditions.push(cond);
            next_idx = new_idx;
        }

        // Date filters
        if let Some(ref after) = payload.published_after {
            let ts = parse_rfc3339_to_timestamp(after)?;
            conditions.push(format!("e.published_at >= ?{}", next_idx));
            params.push(SqlParam::from_i64(ts));
            next_idx += 1;
        }
        if let Some(ref before) = payload.published_before {
            let ts = parse_rfc3339_to_timestamp(before)?;
            conditions.push(format!("e.published_at < ?{}", next_idx));
            params.push(SqlParam::from_i64(ts));
            next_idx += 1;
        }

        // GLOB filters
        if let Some(ref glob) = payload.title_glob {
            conditions.push(format!("e.title GLOB ?{}", next_idx));
            params.push(SqlParam::from_string(glob.clone()));
            next_idx += 1;
        }
        if let Some(ref glob) = payload.content_glob {
            conditions.push(format!("COALESCE(e.content, '') GLOB ?{}", next_idx));
            params.push(SqlParam::from_string(glob.clone()));
            next_idx += 1;
        }
        if let Some(ref glob) = payload.url_glob {
            conditions.push(format!("e.url GLOB ?{}", next_idx));
            params.push(SqlParam::from_string(glob.clone()));
            next_idx += 1;
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conditions.join(" AND "))
        };

        // Count query
        let count_sql = format!("SELECT COUNT(*) FROM entries e{}", where_clause);
        let count: usize = conn
            .prepare(&count_sql)
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row(rusqlite::params_from_iter(params.iter()), |row| row.get(0))?;

        // Data query
        let data_sql = format!(
            "SELECT e.id, e.feed_id, e.source_id, e.syndication_format, \
             e.guid, e.published_at, e.title, e.url, e.content, \
             e.status_read, e.status_favorite \
             FROM entries e{} ORDER BY e.published_at DESC LIMIT ?{} OFFSET ?{}",
            where_clause,
            next_idx,
            next_idx + 1
        );
        params.push(SqlParam::from_usize(limit));
        params.push(SqlParam::from_usize(offset));

        let entries = conn
            .prepare(&data_sql)
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_map(rusqlite::params_from_iter(params.iter()), |row| {
                Ok(ListEntriesResponseEntry {
                    id: row.get(0)?,
                    feed_id: row.get(1)?,
                    source_id: row.get(2)?,
                    syndication_format: row.get(3)?,
                    guid: row.get(4)?,
                    published_at: chrono::DateTime::from_timestamp_secs(row.get(5)?)
                        .map(|d| d.to_rfc3339()),
                    title: row.get(6)?,
                    url: row.get(7)?,
                    content: row.get(8)?,
                    status_read: row.get(9)?,
                    status_favorite: row.get(10)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .inspect_err(|e| {
                event!(Level::ERROR, "failed to collect search results: {:?}", e);
            })?;

        Ok::<SearchEntriesResponse, SearchEntriesError>(SearchEntriesResponse {
            count,
            offset,
            limit,
            entries,
        })
    })
    .await
    .inspect_err(|e| {
        event!(Level::ERROR, "task error in search_entries: {:?}", e);
    });

    match result {
        Ok(Ok(response)) => Ok(Json(response).into_response()),
        Ok(Err(SearchEntriesError::InvalidDate(msg))) => {
            Err((StatusCode::BAD_REQUEST, format!("Invalid date: {}", msg)).into_response())
        }
        Ok(Err(SearchEntriesError::EmptyTagFilter)) => {
            Err((StatusCode::BAD_REQUEST, "Empty tag filter array").into_response())
        }
        Ok(Err(SearchEntriesError::Database(e))) => {
            event!(Level::ERROR, "database error in search_entries: {:?}", e);
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(_) => Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()),
    }
}
