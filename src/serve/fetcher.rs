use crate::http::USER_AGENT;
use anyhow::Result;
use rss::Channel;

/// Parent function for the fetcher threads.
pub fn fetcher() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
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
        })
}
