use crate::http::USER_AGENT;
use anyhow::Result;
use r2d2::{Pool, PooledConnection};
use r2d2_sqlite::SqliteConnectionManager;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

#[derive(Debug)]
pub enum FetchManagerCommand {
    RefreshFeed(i64),
}

/// Create a manager for the fetcher tasks.
pub async fn manager(
    mut rx: mpsc::Receiver<FetchManagerCommand>,
    pool: Pool<SqliteConnectionManager>,
    token: CancellationToken,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()?;

    // Process commands as they come in
    while let Some(command) = tokio::select! {
        command = rx.recv() => command,
        _ = token.cancelled() => return Ok(()),
    } {
        match command {
            FetchManagerCommand::RefreshFeed(feed_id) => {
                refresh_feed(&client, feed_id, pool.clone()).await?;
            }
        }
    }

    Ok(())
}

/// Refresh the feed corresponding to the provided `feed_id`.
async fn refresh_feed(
    client: &reqwest::Client,
    feed_id: i64,
    pool: Pool<SqliteConnectionManager>,
) -> Result<()> {
    // Get the feed URL and headers from the database
    let conn = pool.get().unwrap();
    let (feed_url, header_etag, header_last_modified, last_checked): (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = conn.query_row(
        "SELECT url, header_etag, header_last_modified, last_checked FROM feeds WHERE id = ?1",
        [feed_id],
        |row| {
            let url: String = row.get(0)?;
            let etag: Option<String> = row.get(1)?;
            let last_modified: Option<String> = row.get(2)?;
            let last_checked: Option<String> = row.get(3)?;
            Ok((url, etag, last_modified, last_checked))
        },
    )?;

    // Check if the feed was last updated recently
    if let Some(last_checked_str) = last_checked {
        if let Ok(last_checked_time) =
            chrono::NaiveDateTime::parse_from_str(&last_checked_str, "%Y-%m-%d %H:%M:%S")
        {
            let now = chrono::Utc::now();
            let duration_since = now.signed_duration_since(last_checked_time.and_utc());
            if duration_since.num_hours() < 3 {
                debug!(
                    "Feed {} was last checked {} seconds ago, skipping update",
                    feed_id,
                    duration_since.num_seconds()
                );
                return Ok(());
            }
        } else {
            warn!(
                "Unable to parse last_checked feed time stored in database for feed {}: {}",
                feed_id, last_checked_str
            );
        }
    }

    // Handle file:// URLs differently
    let feed_content = if feed_url.starts_with("file://") {
        retrieve_file_feed(&feed_url, feed_id, pool.clone())
    } else {
        retrieve_feed(
            &client,
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
        process_atom_feed(feed_id, feed, conn)?;
    } else if let Ok(channel) = rss::Channel::read_from(&content[..]) {
        process_rss_feed(feed_id, channel, conn)?;
    } else {
        warn!(
            "Feed {} ({}) was not detected as a valid RSS or XML feed",
            feed_id, feed_url
        );
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
    let conn = pool.get().unwrap();

    // Build the request with conditional headers if they exist
    let mut request = client.get(feed_url).header("User-Agent", USER_AGENT);

    if let Some(etag) = etag {
        request = request.header("If-None-Match", etag);
    }

    if let Some(last_modified) = last_modified {
        request = request.header("If-Modified-Since", last_modified);
    }

    // Make HTTP request to fetch the feed
    let resp = request.send().await?;

    // Check if the feed was modified
    match resp.status() {
        reqwest::StatusCode::NOT_MODIFIED => {
            info!("Feed {} was not modified since last check", feed_id);
            // Update last_checked timestamp in database
            conn.execute(
                "UPDATE feeds SET last_checked = ?1 WHERE id = ?2",
                (chrono::Utc::now().timestamp(), feed_id),
            )?;
            return Ok(None);
        }
        reqwest::StatusCode::OK => { /* Do nothing */ }
        // For other status codes, log an issue and stop processing
        _ => {
            warn!(
                "Received status code {} while fetching contents for feed {}",
                resp.status(),
                feed_id
            );
            return Ok(None);
        }
    }

    // Update the feed's headers in the database
    let etag = resp.headers().get("etag").and_then(|h| h.to_str().ok());
    let last_modified = resp
        .headers()
        .get("last-modified")
        .and_then(|h| h.to_str().ok());

    {
        let params = (etag, last_modified, chrono::Utc::now().timestamp(), feed_id);
        conn.execute(
            "UPDATE feeds SET header_etag = ?, header_last_modified = ?, last_checked = ? WHERE id = ?",
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
    let conn = pool.get().unwrap();

    // Extract the file path from the URL
    let file_path = feed_url.strip_prefix("file://").unwrap_or(feed_url);

    // Read the file content
    let content = std::fs::read(file_path)
        .map_err(|e| anyhow::anyhow!("Failed to read file {}: {}", file_path, e))?;

    // Update the last_checked timestamp in the database
    conn.execute(
        "UPDATE feeds SET last_checked = ?1 WHERE id = ?2",
        (chrono::Utc::now().timestamp(), feed_id),
    )?;

    Ok(Some(content))
}

fn process_atom_feed(
    feed_id: i64,
    feed: atom_syndication::Feed,
    conn: PooledConnection<SqliteConnectionManager>,
) -> Result<()> {
    info!(
        "Successfully fetched Atom feed {} with {} items",
        feed_id,
        feed.entries().len()
    );

    // Insert or update entries from the Atom feed
    for entry in feed.entries() {
        let params: Vec<Box<dyn rusqlite::ToSql>> = vec![
            Box::new(feed_id),
            Box::new(entry.id().to_string()),
            Box::new(
                entry
                    .published()
                    // Attempt to parse using RFC 2822 first; failing that we resort to
                    // RFC 3339.
                    .map(|d| d.to_utc().timestamp()),
            ),
            Box::new(entry.title().as_str().to_string()),
            Box::new(entry.links().first().map(|l| l.href().to_string())),
            Box::new(
                entry
                    .content()
                    .and_then(|c| c.value())
                    .map(|v| v.to_string()),
            ),
        ];

        // Insert or update the entry in the database
        conn.execute(
            "INSERT OR REPLACE INTO entries (
                feed_id,
                syndication_format,
                guid,
                published_at,
                title,
                url,
                content,
            ) VALUES (?1, 'atom', ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params_from_iter(params),
        )?;
    }

    Ok(())
}

fn process_rss_feed(
    feed_id: i64,
    channel: rss::Channel,
    conn: PooledConnection<SqliteConnectionManager>,
) -> Result<()> {
    info!(
        "Successfully fetched RSS feed {} with {} items",
        feed_id,
        channel.items().len()
    );

    // Insert or update entries from the RSS feed
    for item in channel.items() {
        let timestamp = item
            .pub_date()
            .and_then(|d| chrono::DateTime::parse_from_rfc2822(d).ok())
            .map(|d| d.timestamp())
            .unwrap_or_else(|| chrono::Utc::now().timestamp());
        let params: Vec<Box<dyn rusqlite::ToSql>> = vec![
            Box::new(feed_id),
            Box::new(
                item.guid()
                    .map(|g| g.value().to_string())
                    .unwrap_or_else(|| {
                        // Generate a GUID if none exists
                        format!("rss-{}-{}", timestamp, item.title().unwrap_or("no-title"))
                    }),
            ),
            Box::new(timestamp),
            Box::new(item.title()),
            Box::new(item.link()),
            Box::new(item.description()),
        ];

        // Insert or update the entry in the database
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
            rusqlite::params_from_iter(params),
        )?;
    }

    Ok(())
}
