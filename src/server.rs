use crate::http::USER_AGENT;
use rss::Channel;

pub async fn server() -> Result<(), Box<dyn std::error::Error>> {
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
