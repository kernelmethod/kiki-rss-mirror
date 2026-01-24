use crate::http::USER_AGENT;
use anyhow::Result;
use r2d2_sqlite::SqliteConnectionManager;
use rss::Channel;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
pub enum FetchManagerCommand {
    RefreshFeed(i64),
}

/// Create a manager for the fetcher tasks.
pub async fn manager(
    mut rx: mpsc::Receiver<FetchManagerCommand>,
    pool: r2d2::Pool<SqliteConnectionManager>,
    _token: CancellationToken,
) -> Result<()> {
    let client = reqwest::Client::new();

    // Process commands as they come in
    while let Some(command) = rx.recv().await {
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
                if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
                    println!("Feed {} was not modified since last check", feed_id);
                    // Update last_checked timestamp in database
                    conn.execute(
                        "UPDATE feeds SET last_checked = datetime('now') WHERE id = ?1",
                        [feed_id],
                    )?;
                    continue;
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
                    let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![];
                    if etag_value.is_empty() {
                        params.push(Box::new(None as Option<&str>));
                    } else {
                        params.push(Box::new(Some(etag_value)));
                    }

                    if last_modified_value.is_empty() {
                        params.push(Box::new(None as Option<&str>));
                    } else {
                        params.push(Box::new(Some(last_modified_value)));
                    }

                    params.push(Box::new(feed_id));

                    conn.execute(
                        "UPDATE feeds SET header_etag = ?, header_last_modified = ?, last_checked = datetime('now') WHERE id = ?",
                        rusqlite::params_from_iter(params)
                    )?;
                }

                let content = resp.bytes().await?;
                let channel = Channel::read_from(&content[..])?;

                // Process the feed data (this would typically involve
                // inserting/updating entries in the database)
                // For now, we just parse it and log that we got it
                println!(
                    "Successfully fetched feed {} with {} items",
                    feed_id,
                    channel.items().len()
                );
            }
        }
    }

    Ok(())
}
