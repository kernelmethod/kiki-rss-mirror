use crate::http::USER_AGENT;
use anyhow::Result;
use r2d2_sqlite::SqliteConnectionManager;
use rss::Channel;
use tokio::sync::mpsc;

#[derive(Debug)]
pub enum FetchManagerCommand {}

/// Create a manager for the fetcher tasks.
pub async fn manager(
    _rx: mpsc::Receiver<FetchManagerCommand>,
    _pool: r2d2::Pool<SqliteConnectionManager>,
) -> Result<()> {
    let client = reqwest::Client::new();
    let resp = client
        .get("https://kernelmethod.org/notes/index.xml")
        .header("User-Agent", USER_AGENT)
        .send()
        .await?;

    let content = resp.bytes().await?;
    let chan = Channel::read_from(&content[..])?;

    println!("{chan:#?}");
    Ok(())
}
