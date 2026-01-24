use crate::http::USER_AGENT;
use anyhow::Result;
use r2d2::{Pool, PooledConnection};
use r2d2_sqlite::SqliteConnectionManager;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

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
                // Get the feed URL and headers from the database
                let conn = pool.get().unwrap();
                let (feed_url, header_etag, header_last_modified): (
                    String,
                    Option<String>,
                    Option<String>,
                ) = conn.query_row(
                    "SELECT url, header_etag, header_last_modified FROM feeds WHERE id = ?1",
                    [feed_id],
                    |row| {
                        let url: String = row.get(0)?;
                        let etag: Option<String> = row.get(1)?;
                        let last_modified: Option<String> = row.get(2)?;
                        Ok((url, etag, last_modified))
                    },
                )?;

                // Build the request with conditional headers if they exist
                let mut request = client.get(&feed_url).header("User-Agent", USER_AGENT);

                if let Some(etag) = header_etag {
                    request = request.header("If-None-Match", etag);
                }

                if let Some(last_modified) = header_last_modified {
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
                            "UPDATE feeds SET last_checked = datetime('now') WHERE id = ?1",
                            [feed_id],
                        )?;
                        continue;
                    }
                    reqwest::StatusCode::OK => { /* Do nothing */ }
                    // For other status codes, log an issue and stop processing
                    _ => {
                        warn!(
                            "Received status code {} while fetching contents for feed {}",
                            resp.status(),
                            feed_id
                        );
                        continue;
                    }
                }

                // Update the feed's headers in the database
                let etag = resp
                    .headers()
                    .get("etag")
                    .map(|h| h.to_str().unwrap_or("").to_string());
                let last_modified = resp
                    .headers()
                    .get("last-modified")
                    .map(|h| h.to_str().unwrap_or("").to_string());

                let etag_value = etag.as_ref().map_or("", |s| s.as_str());
                let last_modified_value = last_modified.as_ref().map_or("", |s| s.as_str());

                {
                    let params: Vec<Box<dyn rusqlite::ToSql>> = vec![
                        Box::new(etag_value),
                        Box::new(last_modified_value),
                        Box::new(feed_id),
                    ];

                    conn.execute(
                        "UPDATE feeds SET header_etag = ?, header_last_modified = ?, last_checked = datetime('now') WHERE id = ?",
                        rusqlite::params_from_iter(params)
                    )?;
                }

                let content = resp.bytes().await?;

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
            }
        }
    }

    Ok(())
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
