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

#[cfg(test)]
mod tests;

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
