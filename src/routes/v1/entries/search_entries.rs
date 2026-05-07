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
#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
#[cfg_attr(feature = "mcp", derive(schemars::JsonSchema))]
#[serde(untagged)]
#[schema(no_recursion)]
pub enum TagFilter {
    /// A single tag name.
    Single(String),
    /// Boolean combination of tags.
    Expr(TagExpr),
}

/// Boolean tag expression.
#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
#[cfg_attr(feature = "mcp", derive(schemars::JsonSchema))]
#[schema(no_recursion)]
pub enum TagExpr {
    /// All of these must match (AND semantics).
    #[serde(rename = "and")]
    And(Vec<TagFilter>),
    /// At least one must match (OR semantics).
    #[serde(rename = "or")]
    Or(Vec<TagFilter>),
    /// Entries that do NOT match the inner expression. Entries with no tags
    /// are considered non-matching for the inner expression and are therefore
    /// included.
    #[serde(rename = "not")]
    Not(Box<TagFilter>),
}

/// Request body for the entry search endpoint.
#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
#[cfg_attr(feature = "mcp", derive(schemars::JsonSchema))]
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
    /// FTS5 full-text search query matched against title, content, and url.
    pub query: Option<String>,
    /// Regex pattern matched against title.
    pub title_regex: Option<String>,
    /// Regex pattern matched against content.
    pub content_regex: Option<String>,
    /// Regex pattern matched against url.
    pub url_regex: Option<String>,
    /// Sort order: "published_at" (default) or "relevance" (only valid when `query` is set).
    pub sort: Option<String>,
    /// Number of records to skip (default: 0).
    pub offset: Option<usize>,
    /// Maximum number of records to return (default: 50).
    pub limit: Option<usize>,
}

/// A single entry in the search response.
#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
pub struct SearchEntriesResponseEntry {
    #[serde(flatten)]
    pub entry: ListEntriesResponseEntry,
    /// BM25 relevance score. Only populated when a full-text `query` is provided.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rank: Option<f64>,
}

/// Response from the entry search endpoint.
#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
pub struct SearchEntriesResponse {
    pub entries: Vec<SearchEntriesResponseEntry>,
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

    #[error("invalid regex: {0}")]
    InvalidRegex(String),

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
                    let (cond, next_idx) = build_tag_condition(f, params, current_idx)?;
                    conditions.push(cond);
                    current_idx = next_idx;
                }
                let sql = format!("({})", conditions.join(" AND "));
                Ok((sql, current_idx))
            }
            TagExpr::Not(inner) => {
                let (cond, next_idx) = build_tag_condition(inner, params, idx)?;
                let sql = format!("NOT ({})", cond);
                Ok((sql, next_idx))
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
            TagFilter::Expr(TagExpr::And(_)) | TagFilter::Expr(TagExpr::Not(_)) => {
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
        // Check if any child is an AND or NOT expression (those can't be
        // folded into a single `IN (...)` clause).
        let needs_per_child = filters.iter().any(|f| {
            matches!(
                f,
                TagFilter::Expr(TagExpr::And(_)) | TagFilter::Expr(TagExpr::Not(_))
            )
        });
        if needs_per_child {
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

/// Validate a regex pattern, returning a user-friendly error on failure.
fn validate_regex(pattern: &str) -> Result<(), SearchEntriesError> {
    regex::Regex::new(pattern)
        .map(|_| ())
        .map_err(|e| SearchEntriesError::InvalidRegex(e.to_string()))
}

/// Search entries
///
/// Search for entries using tag filters, date ranges, GLOB patterns,
/// full-text search, and regex filters.
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
    // Validate regex patterns upfront for clear 400 errors
    if let Some(ref pat) = payload.title_regex {
        validate_regex(pat)
            .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()).into_response())?;
    }
    if let Some(ref pat) = payload.content_regex {
        validate_regex(pat)
            .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()).into_response())?;
    }
    if let Some(ref pat) = payload.url_regex {
        validate_regex(pat)
            .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()).into_response())?;
    }
    if let Some(ref sort) = payload.sort {
        match sort.as_str() {
            "published_at" | "relevance" => {}
            _ => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!("invalid sort value: {}", sort),
                )
                    .into_response());
            }
        }
        if sort == "relevance" && payload.query.is_none() {
            return Err((
                StatusCode::BAD_REQUEST,
                "sort by relevance requires a query",
            )
                .into_response());
        }
    }

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

        let use_fts = payload.query.is_some();

        // FTS5 MATCH condition
        if let Some(ref query) = payload.query {
            conditions.push(format!("entries_fts MATCH ?{}", next_idx));
            params.push(SqlParam::from_string(query.clone()));
            next_idx += 1;
        }

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

        // REGEXP filters
        if let Some(ref pat) = payload.title_regex {
            conditions.push(format!("e.title REGEXP ?{}", next_idx));
            params.push(SqlParam::from_string(pat.clone()));
            next_idx += 1;
        }
        if let Some(ref pat) = payload.content_regex {
            conditions.push(format!("COALESCE(e.content, '') REGEXP ?{}", next_idx));
            params.push(SqlParam::from_string(pat.clone()));
            next_idx += 1;
        }
        if let Some(ref pat) = payload.url_regex {
            conditions.push(format!("e.url REGEXP ?{}", next_idx));
            params.push(SqlParam::from_string(pat.clone()));
            next_idx += 1;
        }

        let from_clause = if use_fts {
            "entries e JOIN entries_fts ON e.id = entries_fts.rowid"
        } else {
            "entries e"
        };

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conditions.join(" AND "))
        };

        // Count query
        let count_sql = format!("SELECT COUNT(*) FROM {}{}", from_clause, where_clause);
        let count: usize = conn
            .prepare(&count_sql)
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row(rusqlite::params_from_iter(params.iter()), |row| row.get(0))?;

        // Determine rank column and order clause
        let (rank_expr, order_clause) = if use_fts {
            let sort = payload.sort.as_deref().unwrap_or("published_at");
            let order = if sort == "relevance" {
                "bm25(entries_fts)".to_string()
            } else {
                "e.published_at DESC".to_string()
            };
            ("bm25(entries_fts)", order)
        } else {
            ("NULL", "e.published_at DESC".to_string())
        };

        // Data query
        let data_sql = format!(
            "SELECT e.id, e.feed_id, e.source_id, e.syndication_format, \
             e.guid, e.published_at, e.title, e.url, e.content, \
             {} \
             FROM {}{} ORDER BY {} LIMIT ?{} OFFSET ?{}",
            rank_expr,
            from_clause,
            where_clause,
            order_clause,
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
                let rank: Option<f64> = row.get(9)?;
                Ok(SearchEntriesResponseEntry {
                    entry: ListEntriesResponseEntry {
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
                    },
                    rank,
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
        Ok(Err(SearchEntriesError::InvalidRegex(msg))) => {
            Err((StatusCode::BAD_REQUEST, format!("Invalid regex: {}", msg)).into_response())
        }
        Ok(Err(SearchEntriesError::Database(e))) => {
            event!(Level::ERROR, "database error in search_entries: {:?}", e);
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(_) => Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()),
    }
}
