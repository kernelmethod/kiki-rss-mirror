use crate::db::tags::SystemTag;
use crate::routes::v1::entries::list_entries::{ListEntriesResponseEntry, DEFAULT_LIMIT};
use crate::routes::v1::entries::rows::{
    entry_columns, entry_from_row, load_entry_tags, EntrySort, ENTRY_COLUMN_COUNT,
};
use crate::server::AppState;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use rusqlite::types::ToSqlOutput;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use thiserror::Error;
use tracing::{event, Level};

/// A tag filter expression supporting AND/OR combinations.
#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
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
pub struct SearchEntriesRequest {
    /// Tag filter expression. Supports AND/OR combinations.
    pub tags: Option<TagFilter>,
    /// Only match entries from the feed with this ID.
    pub feed_id: Option<i64>,
    /// Lower bound for published_at (inclusive), RFC3339 string.
    pub published_after: Option<String>,
    /// Upper bound for published_at (exclusive), RFC3339 string.
    pub published_before: Option<String>,
    /// Lower bound for ingested_at, when Kiki first stored the entry
    /// (inclusive), RFC3339 string.
    #[serde(default)]
    pub ingested_after: Option<String>,
    /// Upper bound for ingested_at (exclusive), RFC3339 string.
    #[serde(default)]
    pub ingested_before: Option<String>,
    /// Only match entries with an ID greater than this. With `sort` set to
    /// "id", pass the last ID of one page to get the next.
    #[serde(default)]
    pub since_id: Option<i64>,
    /// Only match entries with an ID less than this. With `sort` set to
    /// "id_desc", pass the last ID of one page to get the next.
    #[serde(default)]
    pub max_id: Option<i64>,
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
    /// Sort order: "published_at" (default, newest first), "id" (ascending ID, the order Kiki
    /// stored the entries in), "id_desc" (descending ID), or "relevance" (only valid when
    /// `query` is set).
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

/// The default number of IDs the entry ID search returns.
pub const DEFAULT_ID_LIMIT: usize = 1000;

/// The most IDs the entry ID search returns; larger limits are clamped to
/// this.
pub const MAX_ID_LIMIT: usize = 10_000;

/// Response from the entry ID search endpoint.
#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
pub struct SearchEntryIdsResponse {
    /// IDs of the matching entries, in the requested order.
    pub ids: Vec<i64>,
    /// Number of entries matching the search, across all pages.
    pub count: usize,
    pub offset: usize,
    /// The limit that was applied.
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
    walked: &HashSet<String>,
) -> Result<(String, usize), SearchEntriesError> {
    match filter {
        // A tag in `walked` is tested entry by entry, so that a page sorted
        // by date can walk the date index and stop once it is full; any
        // other is looked up once, its entries then fetched by id. See
        // [`walked_tags`].
        TagFilter::Single(name) if walked.contains(name) => {
            let sql = format!(
                "EXISTS (SELECT 1 FROM entry_tags et JOIN tags t ON et.tag_id = t.id \
                 WHERE et.entry_id = e.id AND t.name = ?{})",
                idx
            );
            params.push(SqlParam::from_string(name.clone()));
            Ok((sql, idx + 1))
        }
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
                    let (cond, next_idx) = build_tag_condition(f, params, current_idx, walked)?;
                    conditions.push(cond);
                    current_idx = next_idx;
                }
                let sql = format!("({})", conditions.join(" AND "));
                Ok((sql, current_idx))
            }
            TagExpr::Not(inner) => {
                // Leaving out the entries with any of a list of tags, as the
                // web UI does with read and hidden entries on every page, is
                // a correlated `NOT EXISTS`, one index lookup per entry
                // looked at. `NOT (e.id IN (SELECT ...))` would collect the
                // ID of every entry with the tags on each query first, which
                // costs as much for a page of 25 entries as for a full count.
                if let Some(names) = tag_names(inner) {
                    let placeholders: Vec<String> = (idx..idx + names.len())
                        .map(|i| format!("?{}", i))
                        .collect();
                    let sql = format!(
                        "NOT EXISTS (SELECT 1 FROM entry_tags et \
                         JOIN tags t ON et.tag_id = t.id \
                         WHERE et.entry_id = e.id AND t.name IN ({}))",
                        placeholders.join(", ")
                    );
                    let next_idx = idx + names.len();
                    params.extend(names.into_iter().map(SqlParam::from_string));
                    return Ok((sql, next_idx));
                }
                let (cond, next_idx) = build_tag_condition(inner, params, idx, walked)?;
                let sql = format!("NOT ({})", cond);
                Ok((sql, next_idx))
            }
        },
    }
}

/// The most tags in one search whose entries [`walked_tags`] counts.
const MAX_WALK_CANDIDATES: usize = 8;

/// The tags in `payload`'s filter that a page of `limit` results from
/// `offset` is found sooner by testing entry by entry, newest first, than
/// by looking up all of the tag's entries and sorting them.
///
/// Walking the entries finds a page after looking at about
/// `(offset + limit) * entries / tagged` of them, while looking up the
/// tag's entries costs one fetch for each of its `tagged` entries, so a tag
/// is walked when `tagged² > (offset + limit) * entries`: when it is common
/// enough. Counting a tag's entries reads only an index.
///
/// Only searches sorted by date, of every feed, walk: with a full-text
/// query or a feed, the entries come from the search index or the feed's
/// index instead. Names under an OR of names are tested together, and are
/// never walked.
fn walked_tags(
    conn: &rusqlite::Connection,
    payload: &SearchEntriesRequest,
    limit: usize,
    offset: usize,
) -> Result<HashSet<String>, SearchEntriesError> {
    let mut walked = HashSet::new();
    let Some(filter) = &payload.tags else {
        return Ok(walked);
    };
    let sorted_by_date = !matches!(
        payload.sort.as_deref(),
        Some("relevance") | Some("id") | Some("id_desc")
    );
    if payload.query.is_some()
        || payload.feed_id.is_some()
        || !sorted_by_date
        || excludes_read_and_hidden(filter)
    {
        return Ok(walked);
    }
    let mut names = Vec::new();
    single_tags(filter, &mut names);
    names.sort_unstable();
    names.dedup();
    if names.is_empty() {
        return Ok(walked);
    }

    let entries: u64 = conn.query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0))?;
    let wanted = (offset as u128 + limit as u128) * u128::from(entries);
    let mut tagged = conn.prepare_cached(
        "SELECT COUNT(*) FROM entry_tags et JOIN tags t ON t.id = et.tag_id WHERE t.name = ?1",
    )?;
    for name in names.into_iter().take(MAX_WALK_CANDIDATES) {
        let count: u64 = tagged.query_row([&name], |row| row.get(0))?;
        if u128::from(count) * u128::from(count) > wanted {
            walked.insert(name);
        }
    }
    Ok(walked)
}

/// Collect into `out` the tags in `filter` that are tested on their own,
/// rather than as part of an OR of names.
fn single_tags(filter: &TagFilter, out: &mut Vec<String>) {
    match filter {
        TagFilter::Single(name) => out.push(name.clone()),
        TagFilter::Expr(TagExpr::And(filters) | TagExpr::Or(filters)) => {
            for f in filters {
                single_tags(f, out);
            }
        }
        TagFilter::Expr(TagExpr::Not(inner)) => single_tags(inner, out),
    }
}

/// The names in `filter` if it is a single tag or an OR of tags, which an
/// entry matches by having any one of them; `None` for anything else,
/// including an empty OR.
fn tag_names(filter: &TagFilter) -> Option<Vec<String>> {
    match filter {
        TagFilter::Single(name) => Some(vec![name.clone()]),
        TagFilter::Expr(TagExpr::Or(filters)) if !filters.is_empty() => {
            let mut names = Vec::new();
            collect_or_leaves(filters, &mut names).ok()?;
            Some(names)
        }
        TagFilter::Expr(_) => None,
    }
}

/// Whether `filter` matches exactly the entries with neither `system:read`
/// nor `system:hidden`: `{"not": {"or": [...]}}` of those two tags, in
/// either order, or of the two along with repeats of them.
fn excludes_read_and_hidden(filter: &TagFilter) -> bool {
    let TagFilter::Expr(TagExpr::Not(inner)) = filter else {
        return false;
    };
    let Some(names) = tag_names(inner) else {
        return false;
    };
    let read = SystemTag::Read.name();
    let hidden = SystemTag::Hidden.name();
    names.iter().all(|n| n == read || n == hidden)
        && names.iter().any(|n| n == read)
        && names.iter().any(|n| n == hidden)
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
    walked: &HashSet<String>,
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
                let (cond, next_idx) = build_tag_condition(f, params, current_idx, walked)?;
                conditions.push(cond);
                current_idx = next_idx;
            }
            let sql = format!("({})", conditions.join(" OR "));
            return Ok((sql, current_idx));
        }
    }
    tag_filter_to_sql(filter, params, idx, walked)
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

/// Check the parts of a search that can be checked before touching the
/// database, responding with `400 Bad Request` if any are invalid.
fn validate_request(payload: &SearchEntriesRequest) -> Result<(), Response> {
    for pat in [
        &payload.title_regex,
        &payload.content_regex,
        &payload.url_regex,
    ]
    .into_iter()
    .flatten()
    {
        validate_regex(pat)
            .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()).into_response())?;
    }
    if let Some(ref sort) = payload.sort {
        match sort.as_str() {
            "published_at" | "relevance" | "id" | "id_desc" => {}
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
    Ok(())
}

/// A search translated to SQL: the entries it matches are
/// `SELECT ... FROM {from_clause}{where_clause}`, with `params` bound to
/// parameters 1 through `next_idx - 1`.
struct CompiledSearch {
    from_clause: &'static str,
    /// The conditions, for counting the matching entries.
    where_clause: String,
    /// The same conditions, written to find one page of them; see
    /// [`walked_tags`].
    page_where_clause: String,
    params: Vec<SqlParam>,
    next_idx: usize,
    /// An expression for each entry's BM25 relevance, or `NULL` without a
    /// full-text query.
    rank_expr: &'static str,
    order_clause: &'static str,
}

impl CompiledSearch {
    /// Translate `payload`, which [`validate_request`] has accepted. A page
    /// of the results tests the tags in `walked` entry by entry, rather
    /// than looking up their entries; see [`walked_tags`].
    fn new(
        payload: &SearchEntriesRequest,
        walked: &HashSet<String>,
    ) -> Result<Self, SearchEntriesError> {
        let mut conditions: Vec<String> = Vec::new();
        let mut params: Vec<SqlParam> = Vec::new();
        let mut next_idx: usize = 1;
        let mut push = |condition: String, param: SqlParam| {
            conditions.push(condition.replace("?#", &format!("?{next_idx}")));
            params.push(param);
            next_idx += 1;
        };

        let use_fts = payload.query.is_some();

        // FTS5 MATCH condition
        if let Some(ref query) = payload.query {
            push(
                "entries_fts MATCH ?#".into(),
                SqlParam::from_string(query.clone()),
            );
        }

        // Feed filter
        if let Some(feed_id) = payload.feed_id {
            push("e.feed_id = ?#".into(), SqlParam::from_i64(feed_id));
        }

        // Date filters
        if let Some(ref after) = payload.published_after {
            let ts = parse_rfc3339_to_timestamp(after)?;
            push("e.published_at >= ?#".into(), SqlParam::from_i64(ts));
        }
        if let Some(ref before) = payload.published_before {
            let ts = parse_rfc3339_to_timestamp(before)?;
            push("e.published_at < ?#".into(), SqlParam::from_i64(ts));
        }
        if let Some(ref after) = payload.ingested_after {
            let ts = parse_rfc3339_to_timestamp(after)?;
            push("e.ingested_at >= ?#".into(), SqlParam::from_i64(ts));
        }
        if let Some(ref before) = payload.ingested_before {
            let ts = parse_rfc3339_to_timestamp(before)?;
            push("e.ingested_at < ?#".into(), SqlParam::from_i64(ts));
        }

        // ID range
        if let Some(since_id) = payload.since_id {
            push("e.id > ?#".into(), SqlParam::from_i64(since_id));
        }
        if let Some(max_id) = payload.max_id {
            push("e.id < ?#".into(), SqlParam::from_i64(max_id));
        }

        // GLOB filters
        if let Some(ref glob) = payload.title_glob {
            push(
                "e.title GLOB ?#".into(),
                SqlParam::from_string(glob.clone()),
            );
        }
        if let Some(ref glob) = payload.content_glob {
            push(
                "COALESCE(e.content, '') GLOB ?#".into(),
                SqlParam::from_string(glob.clone()),
            );
        }
        if let Some(ref glob) = payload.url_glob {
            push("e.url GLOB ?#".into(), SqlParam::from_string(glob.clone()));
        }

        // REGEXP filters
        if let Some(ref pat) = payload.title_regex {
            push(
                "e.title REGEXP ?#".into(),
                SqlParam::from_string(pat.clone()),
            );
        }
        if let Some(ref pat) = payload.content_regex {
            push(
                "COALESCE(e.content, '') REGEXP ?#".into(),
                SqlParam::from_string(pat.clone()),
            );
        }
        if let Some(ref pat) = payload.url_regex {
            push("e.url REGEXP ?#".into(), SqlParam::from_string(pat.clone()));
        }

        // Tag filter, which numbers its own parameters. Leaving out read and
        // hidden entries, and nothing else, is what the web UI's lists do;
        // `entries.unread_visible` records it, and partial indexes on it list
        // and count those entries without looking at the others.
        //
        // Counting looks at every matching entry however the tags are
        // tested, so it always looks up each tag's entries; only a page can
        // be found sooner by walking the entries newest first.
        let mut page_conditions = conditions.clone();
        if let Some(ref tag_filter) = payload.tags {
            if excludes_read_and_hidden(tag_filter) {
                conditions.push("e.unread_visible = 1".into());
                page_conditions.push("e.unread_visible = 1".into());
            } else {
                let (cond, new_idx) =
                    build_tag_condition(tag_filter, &mut params, next_idx, &HashSet::new())?;
                let page_cond = if walked.is_empty() {
                    cond.clone()
                } else {
                    build_tag_condition(tag_filter, &mut Vec::new(), next_idx, walked)?.0
                };
                conditions.push(cond);
                page_conditions.push(page_cond);
                next_idx = new_idx;
            }
        }

        let from_clause = if use_fts {
            "entries e JOIN entries_fts ON e.id = entries_fts.rowid"
        } else {
            "entries e"
        };

        let where_of = |conditions: &[String]| {
            if conditions.is_empty() {
                String::new()
            } else {
                format!(" WHERE {}", conditions.join(" AND "))
            }
        };
        let where_clause = where_of(&conditions);
        let page_where_clause = where_of(&page_conditions);

        let order_clause = match payload.sort.as_deref() {
            Some("relevance") => "bm25(entries_fts)",
            Some("id") => EntrySort::Id.order_by(),
            Some("id_desc") => EntrySort::IdDesc.order_by(),
            _ => EntrySort::PublishedAt.order_by(),
        };

        Ok(CompiledSearch {
            from_clause,
            where_clause,
            page_where_clause,
            params,
            next_idx,
            rank_expr: if use_fts { "bm25(entries_fts)" } else { "NULL" },
            order_clause,
        })
    }

    /// Count the entries the search matches.
    fn count(&self, conn: &rusqlite::Connection) -> Result<usize, SearchEntriesError> {
        let count_sql = format!(
            "SELECT COUNT(*) FROM {}{}",
            self.from_clause, self.where_clause
        );
        Ok(conn
            .prepare(&count_sql)
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row(rusqlite::params_from_iter(self.params.iter()), |row| {
                row.get(0)
            })?)
    }

    /// Select `columns` from one page of the matching entries, in order,
    /// consuming the search.
    fn page(mut self, columns: &str, limit: usize, offset: usize) -> (String, Vec<SqlParam>) {
        let sql = format!(
            "SELECT {} FROM {}{} ORDER BY {} LIMIT ?{} OFFSET ?{}",
            columns,
            self.from_clause,
            self.page_where_clause,
            self.order_clause,
            self.next_idx,
            self.next_idx + 1
        );
        self.params.push(SqlParam::from_usize(limit));
        self.params.push(SqlParam::from_usize(offset));
        (sql, self.params)
    }
}

/// The response to a search that failed with `e`.
fn error_response(e: SearchEntriesError) -> Response {
    match e {
        SearchEntriesError::InvalidDate(msg) => {
            (StatusCode::BAD_REQUEST, format!("Invalid date: {}", msg)).into_response()
        }
        SearchEntriesError::EmptyTagFilter => {
            (StatusCode::BAD_REQUEST, "Empty tag filter array").into_response()
        }
        SearchEntriesError::InvalidRegex(msg) => {
            (StatusCode::BAD_REQUEST, format!("Invalid regex: {}", msg)).into_response()
        }
        SearchEntriesError::Database(e) => {
            event!(Level::ERROR, "database error in entry search: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
        }
    }
}

/// Search entries
///
/// Search for entries using tag filters, the feed they came from, date
/// ranges, ID ranges, GLOB patterns, full-text search, and regex filters.
/// Entries are sorted newest first unless `sort` says otherwise; entries
/// with the same publication time are ordered by descending ID.
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
    validate_request(&payload)?;

    let offset = payload.offset.unwrap_or(0);
    let limit = payload.limit.unwrap_or(DEFAULT_LIMIT);

    let result = state
        .db
        .read(move |conn| {
            let walked = walked_tags(conn, &payload, limit, offset)?;
            let search = CompiledSearch::new(&payload, &walked)?;
            let count = search.count(conn)?;
            let columns = format!("{}, {}", entry_columns(), search.rank_expr);
            let (sql, params) = search.page(&columns, limit, offset);

            let mut entries = conn
                .prepare(&sql)
                .inspect_err(|e| {
                    event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
                })?
                .query_map(rusqlite::params_from_iter(params.iter()), |row| {
                    Ok(SearchEntriesResponseEntry {
                        entry: entry_from_row(row)?,
                        rank: row.get(ENTRY_COLUMN_COUNT)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()
                .inspect_err(|e| {
                    event!(Level::ERROR, "failed to collect search results: {:?}", e);
                })?;

            let ids: Vec<i64> = entries.iter().map(|e| e.entry.id).collect();
            let mut tags = load_entry_tags(conn, &ids)?;
            for e in &mut entries {
                e.entry.tags = tags.remove(&e.entry.id).unwrap_or_default();
            }

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
        Ok(Err(e)) => Err(error_response(e)),
        Err(_) => Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()),
    }
}

/// Search entry IDs
///
/// Search for entries exactly like the entry search, taking the same request body, but return
/// only the matching entries' IDs. This is the cheap way to learn which entries are unread or
/// saved, e.g. with a `tags` filter of `{"not": "system:read"}` or `"system:saved"`. The limit
/// defaults to 1000 and is clamped to 10000; the response reports the limit that was applied.
#[utoipa::path(
    post,
    path = "/v1/entries/search/ids",
    request_body = SearchEntriesRequest,
    responses(
        (status = 200, description = "IDs of the matching entries", body = SearchEntryIdsResponse),
        (status = 400, description = "Invalid search parameters"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "entries"
)]
pub async fn search_entry_ids(
    State(state): State<AppState>,
    Json(payload): Json<SearchEntriesRequest>,
) -> Result<Response, Response> {
    validate_request(&payload)?;

    let offset = payload.offset.unwrap_or(0);
    let limit = payload.limit.unwrap_or(DEFAULT_ID_LIMIT).min(MAX_ID_LIMIT);

    let result = state
        .db
        .read(move |conn| {
            let walked = walked_tags(conn, &payload, limit, offset)?;
            let search = CompiledSearch::new(&payload, &walked)?;
            let count = search.count(conn)?;
            let (sql, params) = search.page("e.id", limit, offset);

            let ids = conn
                .prepare(&sql)
                .inspect_err(|e| {
                    event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
                })?
                .query_map(rusqlite::params_from_iter(params.iter()), |row| row.get(0))?
                .collect::<Result<Vec<i64>, _>>()?;

            Ok::<SearchEntryIdsResponse, SearchEntriesError>(SearchEntryIdsResponse {
                ids,
                count,
                offset,
                limit,
            })
        })
        .await
        .inspect_err(|e| {
            event!(Level::ERROR, "task error in search_entry_ids: {:?}", e);
        });

    match result {
        Ok(Ok(response)) => Ok(Json(response).into_response()),
        Ok(Err(e)) => Err(error_response(e)),
        Err(_) => Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    fn filter(json: serde_json::Value) -> TagFilter {
        serde_json::from_value(json).expect("a tag filter")
    }

    /// Only a filter that leaves out exactly the read and hidden entries
    /// is answered from `entries.unread_visible`.
    #[test]
    fn test_excludes_read_and_hidden() {
        for json in [
            serde_json::json!({"not": {"or": ["system:read", "system:hidden"]}}),
            serde_json::json!({"not": {"or": ["system:hidden", "system:read"]}}),
            serde_json::json!({"not": {"or": ["system:hidden", {"or": ["system:read", "system:hidden"]}]}}),
        ] {
            assert!(excludes_read_and_hidden(&filter(json.clone())), "{json}");
        }
        for json in [
            serde_json::json!("system:read"),
            serde_json::json!({"not": "system:read"}),
            serde_json::json!({"not": "system:hidden"}),
            serde_json::json!({"not": {"or": ["system:read", "system:saved"]}}),
            serde_json::json!({"not": {"or": ["system:read", "system:hidden", "news"]}}),
            serde_json::json!({"not": {"and": ["system:read", "system:hidden"]}}),
            serde_json::json!({"and": ["news", {"not": {"or": ["system:read", "system:hidden"]}}]}),
        ] {
            assert!(!excludes_read_and_hidden(&filter(json.clone())), "{json}");
        }
    }

    /// A tag is walked when it is common enough that a page of it turns up
    /// sooner walking the entries than looking up all of the tag's.
    #[test]
    fn test_walked_tags() -> anyhow::Result<()> {
        let conn = crate::db::ConnectionBuilder::default()
            .in_memory()
            .create()
            .build()?;
        conn.execute_batch(
            "INSERT INTO feeds (title, url) VALUES ('f', 'http://example.com/f');
             INSERT INTO tags (name) VALUES ('common'), ('rare');",
        )?;
        for i in 1..=200i64 {
            conn.execute(
                "INSERT INTO entries (id, feed_id, syndication_format, guid, published_at, title, url)
                 VALUES (?1, 1, 'rss', ?1, 0, 't', 'u')",
                [i],
            )?;
            conn.execute(
                "INSERT INTO entry_tags (entry_id, tag_id)
                 SELECT ?1, id FROM tags WHERE name = 'common'",
                [i],
            )?;
            if i <= 3 {
                conn.execute(
                    "INSERT INTO entry_tags (entry_id, tag_id)
                     SELECT ?1, id FROM tags WHERE name = 'rare'",
                    [i],
                )?;
            }
        }
        let request = |json: serde_json::Value| -> SearchEntriesRequest {
            serde_json::from_value(json).expect("a search request")
        };
        let walked = |json: serde_json::Value, limit: usize, offset: usize| {
            let mut names: Vec<String> = walked_tags(&conn, &request(json), limit, offset)
                .expect("walked tags")
                .into_iter()
                .collect();
            names.sort();
            names
        };

        let both = serde_json::json!({"tags": {"and": ["common", "rare"]}});
        assert_eq!(walked(both.clone(), 25, 0), ["common"]);
        // Deep enough into the results, even a common tag is looked up.
        assert!(walked(both, 25, 175).is_empty());
        // So is every tag of a full-text search, of a feed, or sorted by id.
        for json in [
            serde_json::json!({"tags": "common", "query": "t"}),
            serde_json::json!({"tags": "common", "feed_id": 1}),
            serde_json::json!({"tags": "common", "sort": "id"}),
        ] {
            assert!(walked(json.clone(), 25, 0).is_empty(), "{json}");
        }
        Ok(())
    }
}
