#![deny(clippy::panic)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::indexing_slicing)]

pub mod cli;
pub mod db;
pub mod fetcher;
pub mod http;
pub mod routes;
pub mod server;

pub mod scripting;

#[cfg(test)]
pub mod test;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about, long_about = None)]
pub struct Cli {
    /// The subcommand that should be run. See [Commands].
    #[command(subcommand)]
    pub command: Commands,
}

/// Subcommands that are available for Kiki.
#[derive(Subcommand)]
pub enum Commands {
    /// Sets up a new Kiki database and configuration files
    Init(cli::init::InitArgs),

    /// Starts the Kiki server
    Serve(cli::serve::ServeArgs),

    /// Migrate the Kiki database schema to the latest version
    Migrate(cli::migrate::MigrateArgs),

    /// Manage the kiki systemd user service
    #[cfg(feature = "systemd")]
    Service(cli::service::ServiceArgs),
}

impl Commands {
    /// Run the selected subcommand.
    pub fn run(&self) -> Result<()> {
        match self {
            Commands::Init(args) => args.run(),
            Commands::Migrate(args) => args.run(),
            Commands::Serve(args) => args.run(),
            #[cfg(feature = "systemd")]
            Commands::Service(args) => args.run(),
        }
    }
}

pub fn main() -> Result<()> {
    let cli = Cli::parse();

    cli.command.run()
}
