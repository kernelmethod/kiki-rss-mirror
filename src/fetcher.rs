use crate::http::USER_AGENT;
use crate::scripting::{FeedEntry, ScriptRunner};
use anyhow::Result;
use chrono::{TimeZone, Utc};
use r2d2::{Pool, PooledConnection};
use r2d2_sqlite::SqliteConnectionManager;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::fmt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// Structured representation of feed fetch errors.
///
/// Serialized to JSON for storage in the database `last_fetch_error` column
/// and included in API responses.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(tag = "type")]
pub enum FetchError {
    /// The response body could not be parsed as RSS or Atom.
    #[serde(rename = "invalid_feed")]
    InvalidFeed { url: String },
    /// The server returned a non-success HTTP status code.
    #[serde(rename = "http_status")]
    HttpStatus { url: String, status: u16 },
    /// The maximum number of redirects was exceeded.
    #[serde(rename = "too_many_redirects")]
    TooManyRedirects { url: String },
    /// A network or other unexpected error occurred.
    #[serde(rename = "other")]
    Other { message: String },
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::InvalidFeed { url } => {
                write!(
                    f,
                    "Response from {} was not detected as a valid RSS or Atom feed",
                    url
                )
            }
            FetchError::HttpStatus { url, status } => {
                write!(f, "Received HTTP status {} while fetching {}", status, url)
            }
            FetchError::TooManyRedirects { url } => {
                write!(f, "Exceeded maximum redirects while fetching {}", url)
            }
            FetchError::Other { message } => write!(f, "{}", message),
        }
    }
}

/// Parsed `Cache-Control` directives relevant to the fetcher.
struct CacheControl {
    /// `max-age=N` — freshness lifetime in seconds.
    max_age: Option<u64>,
    /// `no-cache` — must revalidate; don't skip fetching.
    no_cache: bool,
    /// `no-store` — don't cache at all.
    no_store: bool,
}

impl CacheControl {
    /// Parse a `Cache-Control` header value into the directives we care about.
    fn parse(header: &str) -> Self {
        let mut cc = CacheControl {
            max_age: None,
            no_cache: false,
            no_store: false,
        };

        for directive in header.split(',') {
            let directive = directive.trim();
            if directive.eq_ignore_ascii_case("no-cache") {
                cc.no_cache = true;
            } else if directive.eq_ignore_ascii_case("no-store") {
                cc.no_store = true;
            } else {
                let lower = directive.to_ascii_lowercase();
                if let Some(val) = lower.strip_prefix("max-age=") {
                    cc.max_age = val.trim().parse::<u64>().ok();
                }
            }
        }

        cc
    }
}

#[derive(Debug)]
pub enum FetchManagerCommand {
    RefreshFeed(i64),
    /// Clears the cached [`crate::scripting::lua::LuaScriptRunner`] for every feed,
    /// forcing runners to be rebuilt from the database on the next refresh.
    ReloadScripts,
}

/// Create a manager for the fetcher tasks.
pub async fn manager(
    mut rx: mpsc::Receiver<FetchManagerCommand>,
    pool: Pool<SqliteConnectionManager>,
    token: CancellationToken,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(USER_AGENT)
        .build()?;

    // Single script runner for all feeds, built eagerly from the current database state.
    // Replaced wholesale when a ReloadScripts command is received.
    #[cfg(feature = "lua")]
    let mut runner: Option<crate::scripting::lua::LuaScriptRunner> = build_runner(&pool);

    // Process commands as they come in
    while let Some(command) = tokio::select! {
        command = rx.recv() => command,
        _ = token.cancelled() => return Ok(()),
    } {
        match command {
            FetchManagerCommand::RefreshFeed(feed_id) => {
                #[cfg(feature = "lua")]
                let script_runner: Option<&dyn ScriptRunner> =
                    runner.as_ref().map(|r| r as &dyn ScriptRunner);
                #[cfg(not(feature = "lua"))]
                let script_runner: Option<&dyn ScriptRunner> = None;

                let _ = refresh_feed(&client, feed_id, pool.clone(), script_runner)
                    .await
                    .inspect_err(|e| {
                        error!(
                            "An error occurred while refreshing feed {}: {:?}",
                            feed_id, e
                        );
                        if let Ok(conn) = pool.get() {
                            set_feed_error(
                                &conn,
                                feed_id,
                                &FetchError::Other {
                                    message: format!("{}", e),
                                },
                            );
                        }
                    });
            }

            FetchManagerCommand::ReloadScripts => {
                #[cfg(feature = "lua")]
                {
                    debug!("Reloading LuaScriptRunner from database");
                    runner = build_runner(&pool);
                    info!("LuaScriptRunner reloaded");
                }
            }
        }
    }

    Ok(())
}

/// Load all Lua script source texts from the database.
#[cfg(feature = "lua")]
fn load_all_script_sources(
    conn: &r2d2::PooledConnection<SqliteConnectionManager>,
) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT text FROM scripts ORDER BY id")?;
    let rows = stmt.query_map([], |row| row.get(0))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Build a [`LuaScriptRunner`] from all scripts currently in the database.
///
/// Returns `None` and logs a warning if the runner cannot be constructed.
#[cfg(feature = "lua")]
fn build_runner(
    pool: &Pool<SqliteConnectionManager>,
) -> Option<crate::scripting::lua::LuaScriptRunner> {
    match pool.get() {
        Ok(conn) => match load_all_script_sources(&conn) {
            Ok(sources) => match crate::scripting::lua::LuaScriptRunner::new(&sources) {
                Ok(r) => Some(r),
                Err(e) => {
                    warn!("failed to compile Lua scripts: {}", e);
                    None
                }
            },
            Err(e) => {
                error!("failed to load script sources from database: {}", e);
                None
            }
        },
        Err(e) => {
            error!(
                "failed to get DB connection while building script runner: {}",
                e
            );
            None
        }
    }
}

/// Record a fetch error for a feed in the database.
///
/// The error is serialized to JSON for structured storage.
fn set_feed_error(
    conn: &PooledConnection<SqliteConnectionManager>,
    feed_id: i64,
    fetch_error: &FetchError,
) {
    let json = match serde_json::to_string(fetch_error) {
        Ok(j) => j,
        Err(e) => {
            error!(
                "Failed to serialize fetch error for feed {}: {:?}",
                feed_id, e
            );
            return;
        }
    };
    if let Err(e) = conn.execute(
        "UPDATE feeds SET last_fetch_error = ?1, last_fetch_error_at = ?2 WHERE id = ?3",
        (&json, Utc::now().timestamp(), feed_id),
    ) {
        error!(
            "Failed to persist fetch error for feed {}: {:?}",
            feed_id, e
        );
    }
}

/// Clear any previously recorded fetch error for a feed.
fn clear_feed_error(conn: &PooledConnection<SqliteConnectionManager>, feed_id: i64) {
    if let Err(e) = conn.execute(
        "UPDATE feeds SET last_fetch_error = NULL, last_fetch_error_at = NULL WHERE id = ?1",
        (feed_id,),
    ) {
        error!("Failed to clear fetch error for feed {}: {:?}", feed_id, e);
    }
}

/// Refresh the feed corresponding to the provided `feed_id`.
pub(crate) async fn refresh_feed(
    client: &reqwest::Client,
    feed_id: i64,
    pool: Pool<SqliteConnectionManager>,
    script_runner: Option<&dyn ScriptRunner>,
) -> Result<()> {
    // Get the feed URL and headers from the database
    let conn = pool.get()?;
    let (feed_url, header_etag, header_last_modified, header_expires, last_checked): (
        String,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<i64>,
    ) = conn.query_row(
        "SELECT url, header_etag, header_last_modified, header_expires, last_checked FROM feeds WHERE id = ?1",
        [feed_id],
        |row| {
            let url: String = row.get(0)?;
            let etag: Option<String> = row.get(1)?;
            let last_modified: Option<String> = row.get(2)?;
            let expires: Option<i64> = row.get(3)?;
            let last_checked: Option<i64> = row.get(4)?;
            Ok((url, etag, last_modified, expires, last_checked))
        },
    )?;

    // If the server provided an Expires header, respect it: skip fetching
    // until the declared expiry time has passed.
    if let Some(expires_ts) = header_expires {
        if let Some(expires_dt) = Utc.timestamp_opt(expires_ts, 0).single() {
            let now = Utc::now();
            if now < expires_dt {
                debug!(
                    "Feed {} has not yet expired (expires in {} seconds), skipping update",
                    feed_id,
                    expires_dt.signed_duration_since(now).num_seconds()
                );
                return Ok(());
            }
        }
    }

    // Check if the feed was last updated recently
    if let Some(last_checked_ts) = last_checked {
        if let Some(last_checked_ts) = Utc.timestamp_opt(last_checked_ts, 0).single() {
            let now = Utc::now();
            let duration_since = now.signed_duration_since(last_checked_ts);
            if duration_since.num_hours() < 3 {
                debug!(
                    "Feed {} was last checked {} seconds ago, skipping update",
                    feed_id,
                    duration_since.num_seconds()
                );
                return Ok(());
            }
        }
    }

    // Handle file:// URLs differently
    let feed_content = if feed_url.starts_with("file://") {
        retrieve_file_feed(&feed_url, feed_id, pool.clone())
    } else {
        retrieve_feed(
            client,
            feed_id,
            &feed_url,
            header_etag.as_deref(),
            header_last_modified.as_deref(),
            pool.clone(),
        )
        .await
    }?;

    let content = if let Some(content) = feed_content {
        content
    } else {
        return Ok(());
    };

    // Attempt to parse content as Atom, with fallback to RSS.
    if let Ok(feed) = atom_syndication::Feed::read_from(&content[..]) {
        process_atom_feed(feed_id, feed, conn, script_runner)?;
        clear_feed_error(&pool.get()?, feed_id);
    } else if let Ok(channel) = rss::Channel::read_from(&content[..]) {
        process_rss_feed(feed_id, channel, conn, script_runner)?;
        clear_feed_error(&pool.get()?, feed_id);
    } else {
        let fetch_err = FetchError::InvalidFeed {
            url: feed_url.clone(),
        };
        warn!("Feed {}: {}", feed_id, fetch_err);
        set_feed_error(&pool.get()?, feed_id, &fetch_err);
    }

    Ok(())
}

async fn retrieve_feed(
    client: &reqwest::Client,
    feed_id: i64,
    feed_url: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
    pool: Pool<SqliteConnectionManager>,
) -> Result<Option<Vec<u8>>> {
    let conn = pool.get()?;

    let mut current_url = feed_url.to_string();
    let mut had_permanent_redirect = false;
    let max_redirects = 10;

    let resp = 'redirect: {
        for _ in 0..=max_redirects {
            // Only send conditional headers on the first request
            let mut request = client.get(&current_url);
            if current_url == feed_url {
                if let Some(etag) = etag {
                    request = request.header("If-None-Match", etag);
                }
                if let Some(last_modified) = last_modified {
                    request = request.header("If-Modified-Since", last_modified);
                }
            }

            let resp = request.send().await?;

            if resp.status().is_redirection() && resp.status() != reqwest::StatusCode::NOT_MODIFIED
            {
                let location = resp
                    .headers()
                    .get("location")
                    .and_then(|h| h.to_str().ok())
                    .ok_or_else(|| anyhow::anyhow!("Redirect response missing Location header"))?
                    .to_string();

                // 301 Moved Permanently and 308 Permanent Redirect both indicate a
                // permanent move
                if resp.status() == reqwest::StatusCode::MOVED_PERMANENTLY
                    || resp.status() == reqwest::StatusCode::PERMANENT_REDIRECT
                {
                    had_permanent_redirect = true;
                }

                // Resolve the Location against the current URL to handle relative redirects
                let base = Url::parse(&current_url)?;
                current_url = base.join(&location)?.to_string();
                continue;
            }

            break 'redirect resp;
        }

        let fetch_err = FetchError::TooManyRedirects {
            url: feed_url.to_string(),
        };
        warn!("Feed {}: {}", feed_id, fetch_err);
        {
            let conn = pool.get()?;
            set_feed_error(&conn, feed_id, &fetch_err);
        }
        return Ok(None);
    };

    // Check if the feed was modified
    match resp.status() {
        reqwest::StatusCode::NOT_MODIFIED => {
            info!("Feed {} was not modified since last check", feed_id);
            // Update last_checked timestamp in database
            conn.execute(
                "UPDATE feeds SET last_checked = ?1 WHERE id = ?2",
                (Utc::now().timestamp(), feed_id),
            )?;
            return Ok(None);
        }
        reqwest::StatusCode::OK => { /* Do nothing */ }
        // For other status codes, log an issue and stop processing
        status => {
            let fetch_err = FetchError::HttpStatus {
                url: feed_url.to_string(),
                status: status.as_u16(),
            };
            warn!("Feed {}: {}", feed_id, fetch_err);
            set_feed_error(&conn, feed_id, &fetch_err);
            return Ok(None);
        }
    }

    // If we followed a permanent redirect, update the stored URL in the database
    if had_permanent_redirect && current_url != feed_url {
        info!(
            "Feed {} permanently redirected from {} to {}; updating stored URL",
            feed_id, feed_url, current_url
        );
        conn.execute(
            "UPDATE feeds SET url = ?1 WHERE id = ?2",
            (&current_url, feed_id),
        )?;
    }

    // Update the feed's headers in the database
    let mut etag: Option<&str> = resp.headers().get("etag").and_then(|h| h.to_str().ok());
    let mut last_modified: Option<&str> = resp
        .headers()
        .get("last-modified")
        .and_then(|h| h.to_str().ok());

    // Parse the Expires header into a Unix timestamp so we can skip future
    // fetches until the declared expiry time has passed.
    let mut expires: Option<i64> = resp
        .headers()
        .get("expires")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| {
            chrono::NaiveDateTime::parse_from_str(s, "%a, %d %b %Y %H:%M:%S GMT")
                .ok()
                .map(|dt| dt.and_utc().timestamp())
        });

    // Parse Cache-Control and apply precedence rules (RFC 7234):
    // - no-store: clear all cache headers
    // - no-cache: allow conditional requests but never skip fetching
    // - max-age: overrides Expires header
    if let Some(cc) = resp
        .headers()
        .get("cache-control")
        .and_then(|h| h.to_str().ok())
        .map(CacheControl::parse)
    {
        if cc.no_store {
            etag = None;
            last_modified = None;
            expires = None;
        } else if cc.no_cache {
            expires = None;
        } else if let Some(max_age) = cc.max_age {
            expires = Some(Utc::now().timestamp() + max_age as i64);
        }
    }

    {
        let params = (
            etag,
            last_modified,
            expires,
            Utc::now().timestamp(),
            feed_id,
        );
        conn.execute(
            "UPDATE feeds SET header_etag = ?, header_last_modified = ?, header_expires = ?, last_checked = ? WHERE id = ?",
            params,
        )?;
    }

    let content = resp.bytes().await?;
    Ok(Some(content.to_vec()))
}

fn retrieve_file_feed(
    feed_url: &str,
    feed_id: i64,
    pool: Pool<SqliteConnectionManager>,
) -> Result<Option<Vec<u8>>> {
    let conn = pool.get()?;

    // Extract the file path from the URL
    let file_path = feed_url.strip_prefix("file://").unwrap_or(feed_url);

    // Read the file content
    let content = std::fs::read(file_path)
        .map_err(|e| anyhow::anyhow!("Failed to read file {}: {}", file_path, e))?;

    // Update the last_checked timestamp in the database
    conn.execute(
        "UPDATE feeds SET last_checked = ?1 WHERE id = ?2",
        (Utc::now().timestamp(), feed_id),
    )?;

    Ok(Some(content))
}

/// Extract a [`FeedEntry`] from an Atom syndication entry.
fn atom_entry_to_feed_entry(feed_id: i64, entry: atom_syndication::Entry) -> FeedEntry {
    FeedEntry {
        feed_id,
        syndication_format: "atom".to_string(),
        guid: entry.id,
        published_at: entry.published.map(|d| d.to_utc().timestamp()),
        title: entry.title.value,
        url: entry.links.into_iter().next().map(|l| l.href),
        content: entry.content.and_then(|c| c.value),
        tags: vec![],
    }
}

/// Extract a [`FeedEntry`] from an RSS item.
fn rss_item_to_feed_entry(feed_id: i64, item: rss::Item) -> FeedEntry {
    let rss::Item {
        pub_date,
        guid,
        title,
        link,
        description,
        ..
    } = item;

    let timestamp = pub_date
        .as_deref()
        .and_then(|d| chrono::DateTime::parse_from_rfc2822(d).ok())
        .map(|d| d.timestamp())
        .unwrap_or_else(|| Utc::now().timestamp());

    let guid = guid.map(|g| g.value).unwrap_or_else(|| {
        // Generate a GUID if none exists
        format!(
            "rss-{}-{}",
            timestamp,
            title.as_deref().unwrap_or("no-title")
        )
    });

    FeedEntry {
        feed_id,
        syndication_format: "rss".to_string(),
        guid,
        published_at: Some(timestamp),
        title: title.unwrap_or_default(),
        url: link,
        content: description,
        tags: vec![],
    }
}

fn process_atom_feed(
    feed_id: i64,
    feed: atom_syndication::Feed,
    conn: PooledConnection<SqliteConnectionManager>,
    script_runner: Option<&dyn ScriptRunner>,
) -> Result<()> {
    info!(
        "Successfully fetched Atom feed {} with {} items",
        feed_id,
        feed.entries.len()
    );

    for entry in feed.entries.into_iter() {
        let feed_entry = atom_entry_to_feed_entry(feed_id, entry);

        let feed_entry = if let Some(runner) = script_runner {
            let original = feed_entry.clone();
            match runner.process_entry(feed_entry) {
                Ok(Some(e)) => e,
                Ok(None) => {
                    debug!("atom entry filtered by script for feed {}", feed_id);
                    continue;
                }
                Err(e) => {
                    warn!(
                        "script error processing atom entry for feed {}: {}; inserting unmodified",
                        feed_id, e
                    );
                    original
                }
            }
        } else {
            feed_entry
        };

        conn.execute(
            "INSERT OR REPLACE INTO entries (
                feed_id,
                syndication_format,
                guid,
                published_at,
                title,
                url,
                content
            ) VALUES (?1, 'atom', ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                feed_id,
                feed_entry.guid,
                feed_entry.published_at,
                feed_entry.title,
                feed_entry.url,
                feed_entry.content
            ],
        )?;

        if !feed_entry.tags.is_empty() {
            sync_entry_tags(&conn, feed_id, &feed_entry.guid, &feed_entry.tags)?;
        }
    }

    Ok(())
}

fn process_rss_feed(
    feed_id: i64,
    channel: rss::Channel,
    conn: PooledConnection<SqliteConnectionManager>,
    script_runner: Option<&dyn ScriptRunner>,
) -> Result<()> {
    info!(
        "Successfully fetched RSS feed {} with {} items",
        feed_id,
        channel.items.len()
    );

    for item in channel.items.into_iter() {
        let feed_entry = rss_item_to_feed_entry(feed_id, item);

        let feed_entry = if let Some(runner) = script_runner {
            let original = feed_entry.clone();
            match runner.process_entry(feed_entry) {
                Ok(Some(e)) => e,
                Ok(None) => {
                    debug!("rss entry filtered by script for feed {}", feed_id);
                    continue;
                }
                Err(e) => {
                    warn!(
                        "script error processing rss entry for feed {}: {}; inserting unmodified",
                        feed_id, e
                    );
                    original
                }
            }
        } else {
            feed_entry
        };

        conn.execute(
            "INSERT OR REPLACE INTO entries (
                feed_id,
                syndication_format,
                guid,
                published_at,
                title,
                url,
                content
            ) VALUES (?1, 'rss', ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                feed_id,
                feed_entry.guid,
                feed_entry.published_at,
                feed_entry.title,
                feed_entry.url,
                feed_entry.content
            ],
        )?;

        if !feed_entry.tags.is_empty() {
            sync_entry_tags(&conn, feed_id, &feed_entry.guid, &feed_entry.tags)?;
        }
    }

    Ok(())
}

/// Resolve and sync the script-provided tags for a newly inserted database entry.
///
/// For each tag name in `tags`:
/// - ensures the tag row exists in `tags` (`INSERT OR IGNORE`)
/// - looks up its `id`
///
/// Then removes any `entry_tags` rows for this entry whose `tag_id` is not in the
/// script-provided set, and inserts new associations (`INSERT OR IGNORE`).
fn sync_entry_tags(
    conn: &PooledConnection<SqliteConnectionManager>,
    feed_id: i64,
    guid: &str,
    tags: &[String],
) -> Result<()> {
    let entry_id: i64 = conn.query_row(
        "SELECT id FROM entries WHERE feed_id = ?1 AND guid = ?2",
        rusqlite::params![feed_id, guid],
        |row| row.get(0),
    )?;

    // Upsert each tag and collect its id.
    let mut tag_ids: Vec<i64> = Vec::with_capacity(tags.len());
    for name in tags {
        conn.execute("INSERT OR IGNORE INTO tags (name) VALUES (?1)", [name])?;
        let id: i64 = conn.query_row(
            "SELECT id FROM tags WHERE name = ?1",
            [name.as_str()],
            |row| row.get(0),
        )?;
        tag_ids.push(id);
    }

    // Remove stale entry_tags rows (those not in the script-provided set).
    if tag_ids.is_empty() {
        conn.execute("DELETE FROM entry_tags WHERE entry_id = ?1", [entry_id])?;
    } else {
        let placeholders = tag_ids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 2))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "DELETE FROM entry_tags WHERE entry_id = ?1 AND tag_id NOT IN ({placeholders})"
        );
        let params: Vec<rusqlite::types::Value> =
            std::iter::once(rusqlite::types::Value::Integer(entry_id))
                .chain(
                    tag_ids
                        .iter()
                        .map(|&id| rusqlite::types::Value::Integer(id)),
                )
                .collect();
        conn.execute(&sql, rusqlite::params_from_iter(params))?;
    }

    // Insert new tag associations.
    for tag_id in tag_ids {
        conn.execute(
            "INSERT OR IGNORE INTO entry_tags (entry_id, tag_id) VALUES (?1, ?2)",
            rusqlite::params![entry_id, tag_id],
        )?;
    }

    Ok(())
}

#[cfg(all(test, feature = "lua"))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::test::TestBuilder;
    use anyhow::Result;
    use rusqlite::OpenFlags;

    fn make_pool(path: &std::path::Path) -> Result<r2d2::Pool<SqliteConnectionManager>> {
        let manager = SqliteConnectionManager::file(path)
            .with_flags(OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .with_init(|c| c.execute_batch("PRAGMA foreign_keys=ON;"));
        Ok(r2d2::Pool::new(manager)?)
    }

    /// Insert a feed and a Lua script linked to it, then return the feed id,
    /// an HTTP client, and a connection pool ready to call [`refresh_feed`].
    async fn setup_feed_with_script(
        tc: &crate::test::TestConfig,
        script_text: &str,
    ) -> Result<(i64, reqwest::Client, r2d2::Pool<SqliteConnectionManager>)> {
        let conn = tc.database_conn()?;
        conn.execute(
            "INSERT INTO feeds (title, url) VALUES ('test feed', ?1)",
            [tc.example_feed_url()],
        )?;
        let feed_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO scripts (engine, text) VALUES ('lua', ?1)",
            [script_text],
        )?;
        let script_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO feed_scripts (feed_id, script_id) VALUES (?1, ?2)",
            rusqlite::params![feed_id, script_id],
        )?;

        let client = reqwest::Client::builder()
            .user_agent(crate::http::USER_AGENT)
            .build()?;
        let pool = make_pool(&tc.database_path())?;

        Ok((feed_id, client, pool))
    }

    /// A filter-all script should result in zero entries being inserted.
    #[tokio::test]
    async fn integration_filter_script_drops_all_entries() -> Result<()> {
        let tc = TestBuilder::default().init_database().build()?;
        let (feed_id, client, pool) =
            setup_feed_with_script(&tc, "return function(entry) return nil end").await?;

        let runner = {
            let conn = pool.get()?;
            let sources = load_all_script_sources(&conn)?;
            crate::scripting::lua::LuaScriptRunner::new(&sources)?
        };
        refresh_feed(&client, feed_id, pool, Some(&runner as &dyn ScriptRunner)).await?;

        let conn = tc.database_conn()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE feed_id = ?1",
            [feed_id],
            |row| row.get(0),
        )?;
        assert_eq!(count, 0, "filter script should have dropped all entries");

        Ok(())
    }

    /// A modifying script should persist its changes to the database.
    #[tokio::test]
    async fn integration_modify_script_changes_titles() -> Result<()> {
        let tc = TestBuilder::default().init_database().build()?;
        let (feed_id, client, pool) = setup_feed_with_script(
            &tc,
            r#"return function(entry) entry.title = "[MODIFIED] " .. entry.title; return entry end"#,
        )
        .await?;

        let runner = {
            let conn = pool.get()?;
            let sources = load_all_script_sources(&conn)?;
            crate::scripting::lua::LuaScriptRunner::new(&sources)?
        };
        refresh_feed(&client, feed_id, pool, Some(&runner as &dyn ScriptRunner)).await?;

        let conn = tc.database_conn()?;
        let mut stmt = conn.prepare("SELECT title FROM entries WHERE feed_id = ?1")?;
        let titles: Vec<String> = stmt
            .query_map([feed_id], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;

        assert!(!titles.is_empty(), "expected entries to be inserted");
        for title in &titles {
            assert!(
                title.starts_with("[MODIFIED] "),
                "title was not modified by script: {title}"
            );
        }

        Ok(())
    }

    /// A tagging script should add the specified tag to every entry.
    #[tokio::test]
    async fn integration_tagging_script_adds_tags() -> Result<()> {
        let tc = TestBuilder::default().init_database().build()?;
        let (feed_id, client, pool) = setup_feed_with_script(
            &tc,
            r#"return function(entry) table.insert(entry.tags, "test-tag"); return entry end"#,
        )
        .await?;

        let runner = {
            let conn = pool.get()?;
            let sources = load_all_script_sources(&conn)?;
            crate::scripting::lua::LuaScriptRunner::new(&sources)?
        };
        refresh_feed(&client, feed_id, pool, Some(&runner as &dyn ScriptRunner)).await?;

        let conn = tc.database_conn()?;
        let entry_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE feed_id = ?1",
            [feed_id],
            |row| row.get(0),
        )?;
        assert!(entry_count > 0, "expected entries to be inserted");

        let tagged_count: i64 = conn.query_row(
            "SELECT COUNT(DISTINCT e.id) FROM entries e
             JOIN entry_tags et ON et.entry_id = e.id
             JOIN tags t ON t.id = et.tag_id
             WHERE e.feed_id = ?1 AND t.name = 'test-tag'",
            [feed_id],
            |row| row.get(0),
        )?;
        assert_eq!(
            entry_count, tagged_count,
            "every entry should have the test-tag"
        );

        Ok(())
    }

    /// When a filter script runs before a tagging script, the filter should
    /// prevent all entries from being inserted and the tagging script should
    /// never run.
    #[tokio::test]
    async fn integration_filter_script_prevents_tagging_script() -> Result<()> {
        let tc = TestBuilder::default().init_database().build()?;

        let conn = tc.database_conn()?;
        conn.execute(
            "INSERT INTO feeds (title, url) VALUES ('test feed', ?1)",
            [tc.example_feed_url()],
        )?;
        let feed_id = conn.last_insert_rowid();

        // Insert the filter script first so it runs first in the chain.
        conn.execute(
            "INSERT INTO scripts (engine, text) VALUES ('lua', 'return function(entry) return nil end')",
            [],
        )?;
        let filter_script_id = conn.last_insert_rowid();

        // Insert the tagging script second.
        conn.execute(
            "INSERT INTO scripts (engine, text) VALUES ('lua', 'return function(entry) table.insert(entry.tags, \"should-not-appear\"); return entry end')",
            [],
        )?;
        let tag_script_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO feed_scripts (feed_id, script_id) VALUES (?1, ?2)",
            rusqlite::params![feed_id, filter_script_id],
        )?;
        conn.execute(
            "INSERT INTO feed_scripts (feed_id, script_id) VALUES (?1, ?2)",
            rusqlite::params![feed_id, tag_script_id],
        )?;

        let client = reqwest::Client::builder()
            .user_agent(crate::http::USER_AGENT)
            .build()?;
        let pool = make_pool(&tc.database_path())?;

        let runner = {
            let conn = pool.get()?;
            let sources = load_all_script_sources(&conn)?;
            crate::scripting::lua::LuaScriptRunner::new(&sources)?
        };
        refresh_feed(&client, feed_id, pool, Some(&runner as &dyn ScriptRunner)).await?;

        let conn = tc.database_conn()?;
        let entry_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE feed_id = ?1",
            [feed_id],
            |row| row.get(0),
        )?;
        assert_eq!(
            entry_count, 0,
            "filter script should have prevented all entries from being inserted"
        );

        let tag_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM tags WHERE name = 'should-not-appear'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(
            tag_count, 0,
            "tagging script should not have run after filter script dropped the entry"
        );

        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod cache_tests {
    use super::*;
    use crate::test::{FeedServerState, SharedFeedServerState, TestBuilder};
    use anyhow::Result;
    use rusqlite::OpenFlags;
    use std::sync::{Arc, Mutex};

    fn make_pool(path: &std::path::Path) -> Result<r2d2::Pool<SqliteConnectionManager>> {
        let manager = SqliteConnectionManager::file(path)
            .with_flags(OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .with_init(|c| c.execute_batch("PRAGMA foreign_keys=ON;"));
        Ok(r2d2::Pool::new(manager)?)
    }

    /// Insert a feed pointing at the test feed server's RSS URL and return
    /// the feed id, an HTTP client, and a connection pool.
    async fn setup_feed_for_cache_test(
        tc: &crate::test::TestConfig,
    ) -> Result<(i64, reqwest::Client, r2d2::Pool<SqliteConnectionManager>)> {
        let conn = tc.database_conn()?;
        conn.execute(
            "INSERT INTO feeds (title, url) VALUES ('cache test feed', ?1)",
            [tc.rss_feed_url()],
        )?;
        let feed_id = conn.last_insert_rowid();

        let client = reqwest::Client::builder()
            .user_agent(crate::http::USER_AGENT)
            .build()?;
        let pool = make_pool(&tc.database_path())?;
        Ok((feed_id, client, pool))
    }

    /// Reset `last_checked` to 4 hours ago so the 3-hour throttle does not
    /// block the next call to `refresh_feed`.
    fn reset_last_checked(conn: &rusqlite::Connection, feed_id: i64) {
        conn.execute(
            "UPDATE feeds SET last_checked = ?1 WHERE id = ?2",
            rusqlite::params![Utc::now().timestamp() - 4 * 3600, feed_id],
        )
        .unwrap();
    }

    /// Server sends ETag → stored in DB → second request sends If-None-Match → gets 304.
    #[tokio::test]
    async fn test_etag_stored_and_sent() -> Result<()> {
        let mut tc = TestBuilder::default().init_database().build()?;
        let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
            etag: Some("\"abc123\"".into()),
            ..Default::default()
        }));
        tc.init_feed_server_with_state(state.clone()).await?;

        let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

        // First fetch: should get 200, store the etag
        refresh_feed(&client, feed_id, pool.clone(), None).await?;

        let conn = tc.database_conn()?;
        let stored_etag: Option<String> = conn.query_row(
            "SELECT header_etag FROM feeds WHERE id = ?1",
            [feed_id],
            |row| row.get(0),
        )?;
        assert_eq!(stored_etag.as_deref(), Some("\"abc123\""));

        // Reset last_checked so the throttle doesn't block us
        reset_last_checked(&conn, feed_id);

        // Second fetch: should send If-None-Match and get 304
        refresh_feed(&client, feed_id, pool, None).await?;

        let s = state.lock().unwrap();
        assert_eq!(
            s.full_response_count, 1,
            "only the first request should get a full response"
        );
        assert_eq!(s.not_modified_count, 1, "second request should get 304");

        Ok(())
    }

    /// Server sends Last-Modified → stored in DB → second request sends
    /// If-Modified-Since → gets 304.
    #[tokio::test]
    async fn test_last_modified_stored_and_sent() -> Result<()> {
        let mut tc = TestBuilder::default().init_database().build()?;
        let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
            last_modified: Some("Sat, 01 Jan 2025 00:00:00 GMT".into()),
            ..Default::default()
        }));
        tc.init_feed_server_with_state(state.clone()).await?;

        let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

        // First fetch: 200, stores Last-Modified
        refresh_feed(&client, feed_id, pool.clone(), None).await?;

        let conn = tc.database_conn()?;
        let stored_lm: Option<String> = conn.query_row(
            "SELECT header_last_modified FROM feeds WHERE id = ?1",
            [feed_id],
            |row| row.get(0),
        )?;
        assert_eq!(stored_lm.as_deref(), Some("Sat, 01 Jan 2025 00:00:00 GMT"));

        reset_last_checked(&conn, feed_id);

        // Second fetch: should send If-Modified-Since and get 304
        refresh_feed(&client, feed_id, pool, None).await?;

        let s = state.lock().unwrap();
        assert_eq!(s.full_response_count, 1);
        assert_eq!(s.not_modified_count, 1);

        Ok(())
    }

    /// A future Expires header causes refresh_feed to skip the HTTP request entirely.
    #[tokio::test]
    async fn test_expires_skips_fetch() -> Result<()> {
        let future_expires = (Utc::now() + chrono::Duration::hours(1))
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string();

        let mut tc = TestBuilder::default().init_database().build()?;
        let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
            expires: Some(future_expires),
            ..Default::default()
        }));
        tc.init_feed_server_with_state(state.clone()).await?;

        let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

        // First fetch: 200, stores Expires timestamp
        refresh_feed(&client, feed_id, pool.clone(), None).await?;

        let conn = tc.database_conn()?;
        let stored_expires: Option<i64> = conn.query_row(
            "SELECT header_expires FROM feeds WHERE id = ?1",
            [feed_id],
            |row| row.get(0),
        )?;
        assert!(stored_expires.is_some(), "expires should be stored in DB");

        reset_last_checked(&conn, feed_id);

        // Second call: should skip entirely due to unexpired Expires header
        refresh_feed(&client, feed_id, pool, None).await?;

        let s = state.lock().unwrap();
        assert_eq!(
            s.request_count, 1,
            "only one HTTP request should have been made; second should be skipped"
        );

        Ok(())
    }

    /// Once the Expires time has passed, the fetcher makes a new request.
    #[tokio::test]
    async fn test_expired_expires_allows_fetch() -> Result<()> {
        let past_expires = (Utc::now() - chrono::Duration::seconds(1))
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string();

        let mut tc = TestBuilder::default().init_database().build()?;
        let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
            expires: Some(past_expires),
            ..Default::default()
        }));
        tc.init_feed_server_with_state(state.clone()).await?;

        let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

        // First fetch: 200, stores the already-past Expires timestamp
        refresh_feed(&client, feed_id, pool.clone(), None).await?;

        let conn = tc.database_conn()?;
        reset_last_checked(&conn, feed_id);

        // Second call: expires is in the past, so a new HTTP request should be made
        refresh_feed(&client, feed_id, pool, None).await?;

        let s = state.lock().unwrap();
        assert_eq!(
            s.request_count, 2,
            "both calls should have made HTTP requests since Expires is in the past"
        );

        Ok(())
    }

    /// On 304 Not Modified, `last_checked` is updated but no new entries are inserted.
    #[tokio::test]
    async fn test_304_updates_last_checked_only() -> Result<()> {
        let mut tc = TestBuilder::default().init_database().build()?;
        let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
            etag: Some("\"check304\"".into()),
            ..Default::default()
        }));
        tc.init_feed_server_with_state(state.clone()).await?;

        let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

        // First fetch: inserts entries
        refresh_feed(&client, feed_id, pool.clone(), None).await?;

        let conn = tc.database_conn()?;
        let entry_count_after_first: i64 = conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE feed_id = ?1",
            [feed_id],
            |row| row.get(0),
        )?;
        assert!(
            entry_count_after_first > 0,
            "first fetch should insert entries"
        );

        reset_last_checked(&conn, feed_id);

        // Second fetch: 304, should not change entry count
        refresh_feed(&client, feed_id, pool, None).await?;

        let entry_count_after_second: i64 = conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE feed_id = ?1",
            [feed_id],
            |row| row.get(0),
        )?;
        assert_eq!(
            entry_count_after_first, entry_count_after_second,
            "304 should not insert new entries"
        );

        // last_checked should be recent (within the last 10 seconds)
        let last_checked: i64 = conn.query_row(
            "SELECT last_checked FROM feeds WHERE id = ?1",
            [feed_id],
            |row| row.get(0),
        )?;
        let now = Utc::now().timestamp();
        assert!(
            now - last_checked < 10,
            "last_checked should have been updated to a recent time"
        );

        Ok(())
    }

    /// When both etag and last_modified are stored, both conditional headers
    /// are sent on the next request.
    #[tokio::test]
    async fn test_etag_and_last_modified_both_sent() -> Result<()> {
        let mut tc = TestBuilder::default().init_database().build()?;
        let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
            etag: Some("\"both-test\"".into()),
            last_modified: Some("Sun, 02 Feb 2025 12:00:00 GMT".into()),
            ..Default::default()
        }));
        tc.init_feed_server_with_state(state.clone()).await?;

        let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

        // First fetch: stores both headers
        refresh_feed(&client, feed_id, pool.clone(), None).await?;

        let conn = tc.database_conn()?;
        let (stored_etag, stored_lm): (Option<String>, Option<String>) = conn.query_row(
            "SELECT header_etag, header_last_modified FROM feeds WHERE id = ?1",
            [feed_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(stored_etag.as_deref(), Some("\"both-test\""));
        assert_eq!(stored_lm.as_deref(), Some("Sun, 02 Feb 2025 12:00:00 GMT"));

        reset_last_checked(&conn, feed_id);

        // Second fetch: both conditional headers sent, server returns 304
        refresh_feed(&client, feed_id, pool, None).await?;

        let s = state.lock().unwrap();
        assert_eq!(s.full_response_count, 1);
        assert_eq!(s.not_modified_count, 1);
        assert_eq!(s.request_count, 2);

        Ok(())
    }

    /// Cache-Control: max-age=3600 stores header_expires ≈ now+3600 and skips
    /// the second fetch.
    #[tokio::test]
    async fn test_max_age_sets_expires() -> Result<()> {
        let mut tc = TestBuilder::default().init_database().build()?;
        let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
            cache_control: Some("max-age=3600".into()),
            ..Default::default()
        }));
        tc.init_feed_server_with_state(state.clone()).await?;

        let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;
        let before = Utc::now().timestamp();

        refresh_feed(&client, feed_id, pool.clone(), None).await?;

        let conn = tc.database_conn()?;
        let stored_expires: Option<i64> = conn.query_row(
            "SELECT header_expires FROM feeds WHERE id = ?1",
            [feed_id],
            |row| row.get(0),
        )?;
        let expires = stored_expires.expect("header_expires should be set");
        assert!(
            expires >= before + 3600 && expires <= before + 3600 + 5,
            "header_expires should be approximately now + 3600, got offset {}",
            expires - before
        );

        reset_last_checked(&conn, feed_id);

        // Second fetch: should be skipped because max-age hasn't expired
        refresh_feed(&client, feed_id, pool, None).await?;

        let s = state.lock().unwrap();
        assert_eq!(
            s.request_count, 1,
            "second fetch should be skipped due to max-age"
        );

        Ok(())
    }

    /// Cache-Control: max-age takes precedence over a past Expires header.
    #[tokio::test]
    async fn test_max_age_overrides_expires() -> Result<()> {
        let past_expires = (Utc::now() - chrono::Duration::seconds(60))
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string();

        let mut tc = TestBuilder::default().init_database().build()?;
        let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
            expires: Some(past_expires),
            cache_control: Some("max-age=3600".into()),
            ..Default::default()
        }));
        tc.init_feed_server_with_state(state.clone()).await?;

        let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;
        let before = Utc::now().timestamp();

        refresh_feed(&client, feed_id, pool.clone(), None).await?;

        let conn = tc.database_conn()?;
        let stored_expires: Option<i64> = conn.query_row(
            "SELECT header_expires FROM feeds WHERE id = ?1",
            [feed_id],
            |row| row.get(0),
        )?;
        let expires = stored_expires.expect("header_expires should be set");
        // max-age should win over the past Expires header
        assert!(
            expires >= before + 3600,
            "max-age should override past Expires; got {} which is only {} from now",
            expires,
            expires - before
        );

        Ok(())
    }

    /// Cache-Control: no-cache clears header_expires but preserves ETag for
    /// conditional requests.
    #[tokio::test]
    async fn test_no_cache_clears_expires() -> Result<()> {
        let mut tc = TestBuilder::default().init_database().build()?;
        let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
            etag: Some("\"no-cache-test\"".into()),
            cache_control: Some("no-cache".into()),
            ..Default::default()
        }));
        tc.init_feed_server_with_state(state.clone()).await?;

        let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

        refresh_feed(&client, feed_id, pool.clone(), None).await?;

        let conn = tc.database_conn()?;
        let (stored_etag, stored_expires): (Option<String>, Option<i64>) = conn.query_row(
            "SELECT header_etag, header_expires FROM feeds WHERE id = ?1",
            [feed_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(
            stored_etag.as_deref(),
            Some("\"no-cache-test\""),
            "ETag should be preserved with no-cache"
        );
        assert!(
            stored_expires.is_none(),
            "header_expires should be NULL with no-cache"
        );

        reset_last_checked(&conn, feed_id);

        // Second fetch: should make an HTTP request (no skip) and get 304
        refresh_feed(&client, feed_id, pool, None).await?;

        let s = state.lock().unwrap();
        assert_eq!(s.full_response_count, 1);
        assert_eq!(
            s.not_modified_count, 1,
            "conditional request should get 304"
        );

        Ok(())
    }

    /// Cache-Control: no-store clears all cache headers.
    #[tokio::test]
    async fn test_no_store_clears_all_cache_headers() -> Result<()> {
        let mut tc = TestBuilder::default().init_database().build()?;
        let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
            etag: Some("\"no-store-test\"".into()),
            last_modified: Some("Sat, 01 Jan 2025 00:00:00 GMT".into()),
            cache_control: Some("no-store".into()),
            ..Default::default()
        }));
        tc.init_feed_server_with_state(state.clone()).await?;

        let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

        refresh_feed(&client, feed_id, pool, None).await?;

        let conn = tc.database_conn()?;
        let (stored_etag, stored_lm, stored_expires): (
            Option<String>,
            Option<String>,
            Option<i64>,
        ) = conn.query_row(
            "SELECT header_etag, header_last_modified, header_expires FROM feeds WHERE id = ?1",
            [feed_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert!(
            stored_etag.is_none(),
            "etag should be cleared with no-store"
        );
        assert!(
            stored_lm.is_none(),
            "last_modified should be cleared with no-store"
        );
        assert!(
            stored_expires.is_none(),
            "expires should be cleared with no-store"
        );

        Ok(())
    }

    /// After no-store clears headers, the second fetch sends no conditional
    /// headers, resulting in two full 200 responses.
    #[tokio::test]
    async fn test_no_store_prevents_conditional_request() -> Result<()> {
        let mut tc = TestBuilder::default().init_database().build()?;
        let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
            etag: Some("\"no-store-cond\"".into()),
            cache_control: Some("no-store".into()),
            ..Default::default()
        }));
        tc.init_feed_server_with_state(state.clone()).await?;

        let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

        // First fetch: 200 (no-store clears stored etag)
        refresh_feed(&client, feed_id, pool.clone(), None).await?;

        let conn = tc.database_conn()?;
        reset_last_checked(&conn, feed_id);

        // Second fetch: no conditional headers sent, so another 200
        refresh_feed(&client, feed_id, pool, None).await?;

        let s = state.lock().unwrap();
        assert_eq!(
            s.full_response_count, 2,
            "both requests should get full 200 responses"
        );
        assert_eq!(
            s.not_modified_count, 0,
            "no 304 should occur since no-store cleared conditional headers"
        );

        Ok(())
    }

    /// A gzip-compressed response is transparently decompressed by reqwest,
    /// and the feed entries are correctly parsed and inserted.
    #[tokio::test]
    async fn test_gzip_compressed_response() -> Result<()> {
        let mut tc = TestBuilder::default().init_database().build()?;
        let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
            content_encoding: Some("gzip".into()),
            ..Default::default()
        }));
        tc.init_feed_server_with_state(state.clone()).await?;

        let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

        refresh_feed(&client, feed_id, pool, None).await?;

        let conn = tc.database_conn()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE feed_id = ?1",
            [feed_id],
            |row| row.get(0),
        )?;
        assert!(
            count > 0,
            "entries should be inserted from gzip-compressed response"
        );

        let s = state.lock().unwrap();
        assert_eq!(s.full_response_count, 1);

        Ok(())
    }

    /// A deflate-compressed response is transparently decompressed by reqwest,
    /// and the feed entries are correctly parsed and inserted.
    #[tokio::test]
    async fn test_deflate_compressed_response() -> Result<()> {
        let mut tc = TestBuilder::default().init_database().build()?;
        let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
            content_encoding: Some("deflate".into()),
            ..Default::default()
        }));
        tc.init_feed_server_with_state(state.clone()).await?;

        let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

        refresh_feed(&client, feed_id, pool, None).await?;

        let conn = tc.database_conn()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE feed_id = ?1",
            [feed_id],
            |row| row.get(0),
        )?;
        assert!(
            count > 0,
            "entries should be inserted from deflate-compressed response"
        );

        let s = state.lock().unwrap();
        assert_eq!(s.full_response_count, 1);

        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod fetch_error_tests {
    use super::*;
    use crate::test::TestBuilder;
    use anyhow::Result;
    use axum::{routing::get, Router};
    use rusqlite::OpenFlags;

    fn make_pool(path: &std::path::Path) -> Result<r2d2::Pool<SqliteConnectionManager>> {
        let manager = SqliteConnectionManager::file(path)
            .with_flags(OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .with_init(|c| c.execute_batch("PRAGMA foreign_keys=ON;"));
        Ok(r2d2::Pool::new(manager)?)
    }

    /// Helper: insert a feed pointing at the given URL and return the feed id,
    /// an HTTP client (with manual redirect policy), and a connection pool.
    fn setup_feed(
        tc: &crate::test::TestConfig,
        feed_url: &str,
    ) -> Result<(i64, reqwest::Client, r2d2::Pool<SqliteConnectionManager>)> {
        let conn = tc.database_conn()?;
        conn.execute(
            "INSERT INTO feeds (title, url) VALUES ('error test feed', ?1)",
            [feed_url],
        )?;
        let feed_id = conn.last_insert_rowid();

        // Use Policy::none() to match the production client in `manager()`,
        // so that redirect handling is done by our code, not reqwest.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(crate::http::USER_AGENT)
            .build()?;
        let pool = make_pool(&tc.database_path())?;
        Ok((feed_id, client, pool))
    }

    /// Read the stored FetchError JSON from the database for the given feed.
    fn read_stored_error(
        conn: &rusqlite::Connection,
        feed_id: i64,
    ) -> Result<(Option<FetchError>, Option<i64>)> {
        let (json, at): (Option<String>, Option<i64>) = conn.query_row(
            "SELECT last_fetch_error, last_fetch_error_at FROM feeds WHERE id = ?1",
            [feed_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let error = json.and_then(|s| serde_json::from_str(&s).ok());
        Ok((error, at))
    }

    /// When a server returns HTML instead of a feed, the fetcher stores an
    /// `InvalidFeed` error and inserts no entries.
    #[tokio::test]
    async fn test_invalid_feed_error() -> Result<()> {
        let tc = TestBuilder::default().init_database().build()?;

        let app = Router::new().route(
            "/feed",
            get(|| async {
                (
                    [("content-type", "text/html")],
                    "<html><body>Not a feed</body></html>",
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        tokio::spawn(async move { axum::serve(listener, app).await.ok() });

        let feed_url = format!("http://{}/feed", addr);
        let (feed_id, client, pool) = setup_feed(&tc, &feed_url)?;

        refresh_feed(&client, feed_id, pool, None).await?;

        let conn = tc.database_conn()?;
        let entry_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE feed_id = ?1",
            [feed_id],
            |row| row.get(0),
        )?;
        assert_eq!(entry_count, 0, "HTML response should not produce entries");

        let (error, error_at) = read_stored_error(&conn, feed_id)?;
        assert_eq!(
            error,
            Some(FetchError::InvalidFeed {
                url: feed_url.clone()
            }),
        );
        assert!(error_at.is_some());

        Ok(())
    }

    /// When a server returns a non-success HTTP status, the fetcher stores an
    /// `HttpStatus` error.
    #[tokio::test]
    async fn test_http_status_error() -> Result<()> {
        use axum::http::StatusCode;

        let tc = TestBuilder::default().init_database().build()?;

        let app = Router::new().route("/feed", get(|| async { StatusCode::INTERNAL_SERVER_ERROR }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        tokio::spawn(async move { axum::serve(listener, app).await.ok() });

        let feed_url = format!("http://{}/feed", addr);
        let (feed_id, client, pool) = setup_feed(&tc, &feed_url)?;

        refresh_feed(&client, feed_id, pool, None).await?;

        let conn = tc.database_conn()?;
        let (error, error_at) = read_stored_error(&conn, feed_id)?;
        assert_eq!(
            error,
            Some(FetchError::HttpStatus {
                url: feed_url.clone(),
                status: 500,
            }),
        );
        assert!(error_at.is_some());

        Ok(())
    }

    /// When a server sends an endless redirect loop, the fetcher stores a
    /// `TooManyRedirects` error.
    #[tokio::test]
    async fn test_too_many_redirects_error() -> Result<()> {
        let tc = TestBuilder::default().init_database().build()?;

        // Server that always redirects back to itself.
        let app = Router::new().route(
            "/feed",
            get(|| async {
                (
                    axum::http::StatusCode::MOVED_PERMANENTLY,
                    [("location", "/feed")],
                    "",
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        tokio::spawn(async move { axum::serve(listener, app).await.ok() });

        let feed_url = format!("http://{}/feed", addr);
        let (feed_id, client, pool) = setup_feed(&tc, &feed_url)?;

        refresh_feed(&client, feed_id, pool, None).await?;

        let conn = tc.database_conn()?;
        let (error, error_at) = read_stored_error(&conn, feed_id)?;
        assert_eq!(
            error,
            Some(FetchError::TooManyRedirects {
                url: feed_url.clone(),
            }),
        );
        assert!(error_at.is_some());

        Ok(())
    }

    /// After an error is stored, a successful fetch clears it.
    #[tokio::test]
    async fn test_successful_fetch_clears_error() -> Result<()> {
        let tc = TestBuilder::default().init_database().build()?;

        // Start with a server returning HTML (causes InvalidFeed error).
        let app = Router::new().route(
            "/feed",
            get(|| async {
                (
                    [("content-type", "text/html")],
                    "<html><body>Not a feed</body></html>",
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        tokio::spawn(async move { axum::serve(listener, app).await.ok() });

        let feed_url = format!("http://{}/feed", addr);
        let (feed_id, client, pool) = setup_feed(&tc, &feed_url)?;

        refresh_feed(&client, feed_id, pool, None).await?;

        let conn = tc.database_conn()?;
        let (error, _) = read_stored_error(&conn, feed_id)?;
        assert!(error.is_some(), "error should be set after HTML response");

        // Now point the feed at a valid RSS source and refresh again.
        let valid_url = tc.example_feed_url();
        conn.execute(
            "UPDATE feeds SET url = ?1, last_checked = NULL WHERE id = ?2",
            rusqlite::params![valid_url, feed_id],
        )?;

        let pool2 = make_pool(&tc.database_path())?;
        refresh_feed(&client, feed_id, pool2, None).await?;

        let (error, error_at) = read_stored_error(&conn, feed_id)?;
        assert!(
            error.is_none(),
            "error should be cleared after successful fetch"
        );
        assert!(
            error_at.is_none(),
            "error_at should be cleared after successful fetch"
        );

        Ok(())
    }
}
