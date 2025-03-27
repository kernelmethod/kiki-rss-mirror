pub mod cli;
pub mod http;

use crate::cli::{Cli, Commands};
use crate::http::USER_AGENT;
use clap::Parser;
use rss::Channel;

pub fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Server {} => {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(server())?;
        }
    }

    Ok(())
}

async fn server() -> Result<(), Box<dyn std::error::Error>> {
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
